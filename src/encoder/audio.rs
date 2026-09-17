//! Encodeur audio AAC, avec asservissement de l'horloge audio.
//!
//! # Le probleme du drift
//!
//! L'horloge d'une carte son n'est pas exactement a 48 000 Hz : elle derive
//! typiquement de 10 a 100 parties par million par rapport a l'horloge
//! systeme. Sur une heure, 50 ppm font **180 ms** de decalage — largement
//! audible, et fatal pour un enregistrement de conference ou de jeu.
//!
//! Deux approches naives echouent :
//!
//! - **PTS = compteur d'echantillons seul.** Le fichier suppose 48 000
//!   echantillons par seconde ; si la carte en produit 48 002, l'audio prend
//!   du retard sur la video, indefiniment.
//! - **PTS = horodatage de reception.** Chaque fragment porte le jitter de
//!   l'ordonnanceur (±2 ms), ce qui produit des PTS non monotones et des
//!   micro-coupures a l'ecoute.
//!
//! # Ce que l'on fait
//!
//! Le PTS reste derive du compteur d'echantillons — continu, sans jitter.
//! Mais la *cadence* est corrigee : on compare en permanence le temps media
//! audio au temps monotone, et l'ecart pilote
//! [`swr_set_compensation`](https://ffmpeg.org/doxygen/trunk/group__lswr.html),
//! qui etire ou comprime imperceptiblement le signal. L'horloge audio est
//! donc asservie a l'horloge systeme : la derive ne peut pas s'accumuler,
//! quelle que soit la duree de la session.
//!
//! Au-dela d'un seuil (peripherique suspendu, machine en veille), la
//! correction douce n'a plus de sens : on insere du silence ou on saute des
//! echantillons, et on le compte.

use std::ptr;

use ffmpeg_next as ff;
use ff::ffi;
use ff::format::Sample;
use ff::{ChannelLayout, Packet, Rational};

use crate::error::{RecorderError, Result};

use super::{is_again, PacketOut};

/// Parametres de construction de l'encodeur audio.
#[derive(Debug, Clone)]
pub struct AudioEncoderSpec {
    pub sample_rate: u32,
    pub channels: u16,
    pub bitrate: u64,
    pub global_header: bool,
}

/// Resultat d'une demande de correction de derive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftAction {
    /// Ecart negligeable : rien a faire.
    None,
    /// Correction douce appliquee au reechantillonneur.
    Compensated { delta_samples: i32 },
    /// Trou franc : il faut inserer `frames` echantillons de silence.
    InsertSilence { frames: usize },
    /// L'audio est tres en avance : il faut en jeter.
    DropSamples { frames: usize },
}

/// Encodeur AAC.
pub struct AudioEncoder {
    encoder: ff::encoder::audio::Encoder,
    swr: Resampler,
    layout: ChannelLayout,
    rate: u32,
    channels: u16,
    frame_size: usize,
    /// Accumulateur : le reechantillonneur rend un nombre variable
    /// d'echantillons, l'encodeur AAC en exige exactement `frame_size`.
    fifo: AudioFifo,
    /// Tampon de sortie du reechantillonneur, agrandi a la demande.
    scratch: ff::frame::Audio,
    /// Echantillons deja pousses vers l'encodeur : c'est le PTS.
    samples_sent: i64,
    time_base: Rational,
    /// Seuil au-dela duquel on cesse de compenser en douceur.
    max_gap_samples: i64,
}

// SAFETY: l'encodeur vit sur un unique thread ; le contexte swresample qu'il
// detient n'est jamais partage.
unsafe impl Send for AudioEncoder {}

impl AudioEncoder {
    pub fn open(spec: &AudioEncoderSpec, max_gap_ms: u32) -> Result<Self> {
        let codec = ff::encoder::find_by_name("aac").ok_or_else(|| {
            RecorderError::NoUsableEncoder("encodeur AAC absent de cette build ffmpeg".into())
        })?;

        let layout = ChannelLayout::default(spec.channels as i32);
        let time_base = Rational(1, spec.sample_rate as i32);

        let mut audio = ff::codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .map_err(|e| RecorderError::Encode {
                stage: "creation du contexte audio",
                source: e,
            })?;
        audio.set_rate(spec.sample_rate as i32);
        audio.set_channel_layout(layout);
        // L'encodeur AAC natif de ffmpeg ne prend que du flottant planaire.
        audio.set_format(Sample::F32(ff::format::sample::Type::Planar));
        audio.set_bit_rate(spec.bitrate as usize);
        audio.set_time_base(time_base);

        // SAFETY: contexte valide, non encore ouvert.
        unsafe {
            if spec.global_header {
                let ctx = audio.as_mut_ptr();
                (*ctx).flags |= ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32;
            }
        }

        let encoder = audio
            .open_as(codec)
            .map_err(|e| RecorderError::Encode {
                stage: "ouverture de l'encodeur audio",
                source: e,
            })?;

        let frame_size = match encoder.frame_size() {
            0 => 1024,
            n => n as usize,
        };

        let swr = Resampler::new(spec.sample_rate, spec.channels)?;
        let max_gap_samples = spec.sample_rate as i64 * max_gap_ms.max(1) as i64 / 1000;
        let fifo = AudioFifo::new(
            ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP,
            spec.channels as i32,
            (frame_size * 4) as i32,
        )?;
        let mut scratch = ff::frame::Audio::new(
            Sample::F32(ff::format::sample::Type::Planar),
            frame_size * 4,
            layout,
        );
        scratch.set_rate(spec.sample_rate);

        tracing::info!(
            rate = spec.sample_rate,
            channels = spec.channels,
            bitrate_kbps = spec.bitrate / 1000,
            frame_size,
            "encodeur audio AAC ouvert"
        );

        Ok(Self {
            encoder,
            swr,
            layout,
            rate: spec.sample_rate,
            channels: spec.channels,
            frame_size,
            fifo,
            scratch,
            samples_sent: 0,
            time_base,
            max_gap_samples,
        })
    }

    pub fn time_base(&self) -> Rational {
        self.time_base
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Echantillons (par canal) deja remis a l'encodeur.
    pub fn samples_sent(&self) -> i64 {
        self.samples_sent
    }

    pub fn context(&self) -> &ff::codec::Context {
        &self.encoder
    }

    /// Decide quoi faire d'une derive mesuree, sans l'appliquer.
    ///
    /// Fonction pure : c'est elle que testent les tests de synchronisation.
    pub fn plan_drift_correction(&self, drift_samples: i64) -> DriftAction {
        // Un millieme de seconde : sous ce seuil la correction couterait plus
        // qu'elle ne rapporte.
        let deadband = (self.rate / 1000).max(1) as i64;
        if drift_samples.abs() < deadband {
            return DriftAction::None;
        }
        if drift_samples > self.max_gap_samples {
            // L'audio est tres en retard : il manque vraiment du son.
            return DriftAction::InsertSilence {
                frames: drift_samples as usize,
            };
        }
        if -drift_samples > self.max_gap_samples {
            return DriftAction::DropSamples {
                frames: (-drift_samples) as usize,
            };
        }
        // Correction douce : on ne rattrape qu'un huitieme de l'ecart a
        // chaque mesure, et jamais plus de 1 % de la cadence. Une correction
        // brutale s'entend ; celle-ci est inaudible.
        let max_step = (self.rate / 100) as i64;
        let delta = (drift_samples / 8).clamp(-max_step, max_step);
        if delta == 0 {
            DriftAction::None
        } else {
            DriftAction::Compensated {
                delta_samples: delta as i32,
            }
        }
    }

    /// Applique une correction douce au reechantillonneur.
    pub fn apply_compensation(&mut self, delta_samples: i32) -> Result<()> {
        // Repartie sur une seconde : la variation de hauteur est de l'ordre
        // du millieme de demi-ton, donc inaudible.
        self.swr.compensate(delta_samples, self.rate as i32)
    }

    /// Pousse des echantillons entrelaces et encode tout ce qui est complet.
    pub fn push(&mut self, interleaved: &[f32], out: &mut PacketOut<'_>) -> Result<()> {
        let channels = self.channels as usize;
        if channels == 0 || interleaved.is_empty() {
            return Ok(());
        }
        let frames = interleaved.len() / channels;
        self.resample_into_fifo(Some((interleaved, frames)))?;
        self.emit(out, false)
    }

    /// Insere `frames` echantillons de silence, pour combler un trou.
    pub fn push_silence(&mut self, frames: usize, out: &mut PacketOut<'_>) -> Result<()> {
        if frames == 0 {
            return Ok(());
        }
        // Par tranches, pour ne pas allouer plusieurs secondes d'un coup.
        let chunk = self.frame_size.max(1024);
        let silence = vec![0.0f32; chunk * self.channels as usize];
        let mut remaining = frames;
        while remaining > 0 {
            let n = remaining.min(chunk);
            self.resample_into_fifo(Some((&silence[..n * self.channels as usize], n)))?;
            self.emit(out, false)?;
            remaining -= n;
        }
        Ok(())
    }

    /// Jette `frames` echantillons deja accumules (audio trop en avance).
    pub fn drop_samples(&mut self, frames: usize) -> Result<usize> {
        self.fifo.drain(frames)
    }

    /// Fait passer un lot d'echantillons par le reechantillonneur et range le
    /// resultat dans l'accumulateur.
    ///
    /// `swr_convert` peut rendre moins d'echantillons que demande : on ne
    /// suppose jamais le contraire, et c'est exactement pour cela que
    /// l'accumulateur existe. Envoyer directement la sortie du
    /// reechantillonneur a l'encodeur produirait des blocs incomplets, que
    /// l'AAC refuse.
    fn resample_into_fifo(&mut self, input: Option<(&[f32], usize)>) -> Result<()> {
        let in_frames = input.map_or(0, |(_, f)| f);
        let capacity = self
            .swr
            .out_samples_for(in_frames as i32)
            .max(in_frames as i32)
            .max(0) as usize;
        if capacity == 0 {
            return Ok(());
        }
        self.ensure_scratch(capacity);
        let got = self.swr.convert(input, &mut self.scratch, capacity)?;
        if got > 0 {
            self.fifo.write(&self.scratch, got)?;
        }
        Ok(())
    }

    fn ensure_scratch(&mut self, samples: usize) {
        if self.scratch.samples() >= samples {
            return;
        }
        let size = samples.next_power_of_two().max(self.frame_size * 4);
        self.scratch = ff::frame::Audio::new(
            Sample::F32(ff::format::sample::Type::Planar),
            size,
            self.layout,
        );
        self.scratch.set_rate(self.rate);
    }

    /// Encode tous les blocs complets disponibles.
    ///
    /// `final_partial` autorise un dernier bloc incomplet : l'encodeur AAC
    /// l'accepte en fin de flux et le complete lui-meme.
    fn emit(&mut self, out: &mut PacketOut<'_>, final_partial: bool) -> Result<()> {
        while self.fifo.size() >= self.frame_size as i32 {
            self.emit_one(self.frame_size, out)?;
        }
        if final_partial {
            let left = self.fifo.size();
            if left > 0 {
                self.emit_one(left as usize, out)?;
            }
        }
        Ok(())
    }

    fn emit_one(&mut self, samples: usize, out: &mut PacketOut<'_>) -> Result<()> {
        let mut frame = ff::frame::Audio::new(
            Sample::F32(ff::format::sample::Type::Planar),
            self.frame_size,
            self.layout,
        );
        frame.set_rate(self.rate);
        let got = self.fifo.read(&mut frame, samples)?;
        if got == 0 {
            return Ok(());
        }
        if got != self.frame_size {
            frame.set_samples(got);
        }
        // SAFETY: AVFrame valide detenu localement, non encore soumis.
        unsafe {
            (*frame.as_mut_ptr()).pts = self.samples_sent;
        }
        self.samples_sent += got as i64;

        self.encoder
            .send_frame(&frame)
            .map_err(|e| RecorderError::Encode {
                stage: "soumission d'un bloc audio",
                source: e,
            })?;
        self.drain_encoder(out)
    }

    /// Vide le reechantillonneur, puis l'accumulateur, puis l'encodeur.
    pub fn flush(&mut self, out: &mut PacketOut<'_>) -> Result<()> {
        self.resample_into_fifo(None)?;
        self.emit(out, true)?;
        self.encoder.send_eof().map_err(|e| RecorderError::Encode {
            stage: "fin de flux audio",
            source: e,
        })?;
        self.drain_encoder(out)
    }

    fn drain_encoder(&mut self, out: &mut PacketOut<'_>) -> Result<()> {
        loop {
            let mut packet = Packet::empty();
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => out(packet)?,
                Err(e) if is_again(&e) => return Ok(()),
                Err(e) => {
                    return Err(RecorderError::Encode {
                        stage: "recuperation d'un paquet audio",
                        source: e,
                    })
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Reechantillonneur
// ---------------------------------------------------------------------------

/// `SwrContext` : f32 entrelace -> f32 planaire, a cadence identique.
///
/// La conversion de disposition serait triviale a faire a la main ; on passe
/// par swresample precisement pour disposer de `swr_set_compensation`, qui
/// permet d'ajuster la cadence en continu.
struct Resampler {
    ctx: *mut ffi::SwrContext,
    channels: i32,
}

impl Resampler {
    fn new(rate: u32, channels: u16) -> Result<Self> {
        let layout = ChannelLayout::default(channels as i32);
        let mut ctx: *mut ffi::SwrContext = ptr::null_mut();
        // SAFETY: `layout.0` est un `AVChannelLayout` valide ; ffmpeg le copie.
        let rc = unsafe {
            ffi::swr_alloc_set_opts2(
                &mut ctx,
                &layout.0,
                ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP,
                rate as i32,
                &layout.0,
                ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT,
                rate as i32,
                0,
                ptr::null_mut(),
            )
        };
        if rc < 0 || ctx.is_null() {
            return Err(RecorderError::Encode {
                stage: "allocation du reechantillonneur",
                source: ff::Error::from(rc),
            });
        }

        // Cadence d'entree et de sortie identiques : sans ce drapeau,
        // swresample court-circuite le reechantillonneur et
        // `swr_set_compensation` n'aurait aucun effet.
        // SAFETY: le contexte est alloue et pas encore initialise.
        unsafe {
            let name = c"flags";
            ffi::av_opt_set_int(
                ctx as *mut libc::c_void,
                name.as_ptr(),
                ffi::SWR_FLAG_RESAMPLE as i64,
                0,
            );
        }

        // SAFETY: contexte valide, options posees.
        let rc = unsafe { ffi::swr_init(ctx) };
        if rc < 0 {
            // SAFETY: contexte valide dont nous sommes proprietaire.
            unsafe { ffi::swr_free(&mut ctx) };
            return Err(RecorderError::Encode {
                stage: "initialisation du reechantillonneur",
                source: ff::Error::from(rc),
            });
        }

        Ok(Self {
            ctx,
            channels: channels as i32,
        })
    }

    /// Majorant du nombre d'echantillons de sortie pour `in_frames` entrants.
    fn out_samples_for(&self, in_frames: i32) -> i32 {
        // SAFETY: contexte valide.
        unsafe { ffi::swr_get_out_samples(self.ctx, in_frames) }
    }

    /// Convertit, et rend le nombre reel d'echantillons produits.
    fn convert(
        &mut self,
        input: Option<(&[f32], usize)>,
        out: &mut ff::frame::Audio,
        out_capacity: usize,
    ) -> Result<usize> {
        let (in_ptr, in_count) = match input {
            Some((data, frames)) => {
                let expected = frames * self.channels as usize;
                if data.len() < expected {
                    return Err(RecorderError::AudioCapture(format!(
                        "fragment audio incomplet : {} echantillons pour {} attendus",
                        data.len(),
                        expected
                    )));
                }
                (data.as_ptr() as *const u8, frames as i32)
            }
            None => (ptr::null::<u8>(), 0),
        };
        let in_planes = [in_ptr, ptr::null()];
        let in_arg = if in_ptr.is_null() {
            ptr::null()
        } else {
            in_planes.as_ptr()
        };

        // SAFETY: `extended_data` du cadre pointe sur `channels` plans
        // d'au moins `out_capacity` echantillons (garanti par
        // `ensure_scratch`). L'entree, quand elle existe, couvre bien
        // `in_count * channels` flottants : verifie juste au-dessus.
        let rc = unsafe {
            let out_planes = (*out.as_mut_ptr()).extended_data;
            ffi::swr_convert(
                self.ctx,
                out_planes,
                out_capacity as i32,
                in_arg,
                in_count,
            )
        };
        if rc < 0 {
            return Err(RecorderError::Encode {
                stage: "reechantillonnage",
                source: ff::Error::from(rc),
            });
        }
        Ok(rc as usize)
    }

    fn compensate(&mut self, delta: i32, distance: i32) -> Result<()> {
        // SAFETY: contexte valide et initialise avec SWR_FLAG_RESAMPLE.
        let rc = unsafe { ffi::swr_set_compensation(self.ctx, delta, distance.max(1)) };
        if rc < 0 {
            return Err(RecorderError::Encode {
                stage: "compensation de derive",
                source: ff::Error::from(rc),
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Accumulateur
// ---------------------------------------------------------------------------

/// `AVAudioFifo` : file d'echantillons planaires entre le reechantillonneur
/// et l'encodeur.
struct AudioFifo {
    ptr: *mut ffi::AVAudioFifo,
}

impl AudioFifo {
    fn new(fmt: ffi::AVSampleFormat, channels: i32, initial: i32) -> Result<Self> {
        // SAFETY: parametres valides ; la fonction rend NULL en cas d'echec.
        let ptr = unsafe { ffi::av_audio_fifo_alloc(fmt, channels, initial.max(1)) };
        if ptr.is_null() {
            return Err(RecorderError::AudioCapture(
                "allocation de l'accumulateur audio impossible".into(),
            ));
        }
        Ok(Self { ptr })
    }

    fn size(&self) -> i32 {
        // SAFETY: pointeur valide detenu par `self`.
        unsafe { ffi::av_audio_fifo_size(self.ptr) }
    }

    fn write(&mut self, frame: &ff::frame::Audio, samples: usize) -> Result<()> {
        // SAFETY: `extended_data` couvre au moins `samples` echantillons par
        // plan ; l'appelant ne passe jamais plus que ce que `swr_convert` a
        // reellement produit.
        let rc = unsafe {
            let planes = (*frame.as_ptr()).extended_data as *const *mut libc::c_void;
            ffi::av_audio_fifo_write(self.ptr, planes, samples as i32)
        };
        if rc < 0 {
            return Err(RecorderError::Encode {
                stage: "ecriture dans l'accumulateur audio",
                source: ff::Error::from(rc),
            });
        }
        Ok(())
    }

    fn read(&mut self, frame: &mut ff::frame::Audio, samples: usize) -> Result<usize> {
        // SAFETY: le cadre est alloue pour au moins `samples` echantillons.
        let rc = unsafe {
            let planes = (*frame.as_mut_ptr()).extended_data as *const *mut libc::c_void;
            ffi::av_audio_fifo_read(self.ptr, planes, samples as i32)
        };
        if rc < 0 {
            return Err(RecorderError::Encode {
                stage: "lecture de l'accumulateur audio",
                source: ff::Error::from(rc),
            });
        }
        Ok(rc as usize)
    }

    /// Jette des echantillons sans les lire. Rend le nombre reellement jete.
    fn drain(&mut self, samples: usize) -> Result<usize> {
        let available = self.size().max(0) as usize;
        let n = samples.min(available);
        if n == 0 {
            return Ok(0);
        }
        // SAFETY: `n` n'excede jamais la taille courante.
        let rc = unsafe { ffi::av_audio_fifo_drain(self.ptr, n as i32) };
        if rc < 0 {
            return Err(RecorderError::Encode {
                stage: "purge de l'accumulateur audio",
                source: ff::Error::from(rc),
            });
        }
        Ok(n)
    }
}

impl Drop for AudioFifo {
    fn drop(&mut self) {
        // SAFETY: pointeur detenu, libere une seule fois.
        unsafe { ffi::av_audio_fifo_free(self.ptr) };
    }
}

impl Drop for Resampler {
    fn drop(&mut self) {
        // SAFETY: contexte detenu, libere une seule fois.
        unsafe { ffi::swr_free(&mut self.ctx) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> AudioEncoderSpec {
        AudioEncoderSpec {
            sample_rate: 48_000,
            channels: 2,
            bitrate: 192_000,
            global_header: true,
        }
    }

    fn open() -> AudioEncoder {
        ff::init().ok();
        AudioEncoder::open(&spec(), 200).expect("AAC doit etre disponible")
    }

    fn tone(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels)
            .map(|i| ((i as f32) * 0.01).sin() * 0.2)
            .collect()
    }

    #[test]
    fn encoder_opens_with_the_expected_parameters() {
        let enc = open();
        assert_eq!(enc.rate(), 48_000);
        assert_eq!(enc.channels(), 2);
        assert_eq!(enc.time_base(), Rational(1, 48_000));
        assert_eq!(enc.frame_size(), 1024);
    }

    #[test]
    fn packets_carry_sample_exact_increasing_timestamps() {
        let mut enc = open();
        let mut pts_list: Vec<i64> = Vec::new();
        {
            let mut sink: PacketOut = Box::new(|p: Packet| {
                if let Some(pts) = p.pts() {
                    pts_list.push(pts);
                }
                Ok(())
            });
            // 1 seconde de son, par fragments de 10 ms.
            for _ in 0..100 {
                enc.push(&tone(480, 2), &mut sink).expect("push");
            }
            enc.flush(&mut sink).expect("flush");
        }
        assert!(pts_list.len() >= 40, "paquets : {}", pts_list.len());
        // Strictement croissants, pas de recul.
        for w in pts_list.windows(2) {
            assert!(w[1] > w[0], "PTS non monotones : {:?}", w);
        }
        // Et espaces d'exactement une trame AAC.
        assert_eq!(pts_list[1] - pts_list[0], 1024);
        // Une seconde d'entree = ~48000 echantillons remis a l'encodeur.
        let sent = enc.samples_sent();
        assert!(
            (sent - 48_000).abs() < 2048,
            "echantillons remis : {sent} (attendu ~48000)"
        );
    }

    #[test]
    fn small_drift_is_ignored() {
        let enc = open();
        // Moins d'une milliseconde d'ecart.
        assert_eq!(enc.plan_drift_correction(10), DriftAction::None);
        assert_eq!(enc.plan_drift_correction(-10), DriftAction::None);
    }

    #[test]
    fn moderate_drift_is_smoothly_compensated() {
        let enc = open();
        // 50 ms de retard : on n'en rattrape qu'un huitieme a la fois.
        match enc.plan_drift_correction(2_400) {
            DriftAction::Compensated { delta_samples } => {
                assert_eq!(delta_samples, 300);
            }
            other => panic!("attendu une compensation, obtenu {other:?}"),
        }
        // Le signe est conserve quand l'audio est en avance.
        match enc.plan_drift_correction(-2_400) {
            DriftAction::Compensated { delta_samples } => assert_eq!(delta_samples, -300),
            other => panic!("attendu une compensation, obtenu {other:?}"),
        }
    }

    #[test]
    fn compensation_step_is_capped() {
        let enc = open();
        // 150 ms de retard : le pas reste plafonne a 1 % de la cadence.
        match enc.plan_drift_correction(9_000) {
            DriftAction::Compensated { delta_samples } => assert_eq!(delta_samples, 480),
            other => panic!("attendu une compensation, obtenu {other:?}"),
        }
    }

    #[test]
    fn a_real_gap_inserts_silence_instead_of_compensating() {
        let enc = open();
        // 500 ms : au-dela du seuil, la correction douce n'a plus de sens.
        match enc.plan_drift_correction(24_000) {
            DriftAction::InsertSilence { frames } => assert_eq!(frames, 24_000),
            other => panic!("attendu du silence, obtenu {other:?}"),
        }
    }

    #[test]
    fn a_large_advance_drops_samples() {
        let enc = open();
        match enc.plan_drift_correction(-24_000) {
            DriftAction::DropSamples { frames } => assert_eq!(frames, 24_000),
            other => panic!("attendu un rejet, obtenu {other:?}"),
        }
    }

    #[test]
    fn compensation_is_accepted_by_swresample() {
        let mut enc = open();
        // Confirme que SWR_FLAG_RESAMPLE est bien actif : sans lui, cet appel
        // echouerait ou serait sans effet.
        assert!(enc.apply_compensation(600).is_ok());
        assert!(enc.apply_compensation(-600).is_ok());
    }

    #[test]
    fn silence_advances_the_clock_like_real_audio() {
        let mut enc = open();
        {
            let mut sink: PacketOut = Box::new(|_p| Ok(()));
            enc.push_silence(48_000, &mut sink).expect("silence");
        }
        let sent = enc.samples_sent();
        assert!(
            (sent - 48_000).abs() < 2048,
            "le silence doit faire avancer l'horloge : {sent}"
        );
    }

    #[test]
    fn an_incomplete_fragment_is_rejected_not_silently_truncated() {
        let mut enc = open();
        // 3 flottants annonces comme 2 trames stereo : il en faudrait 4.
        let err = enc.resample_into_fifo(Some((&[0.0, 0.0, 0.0], 2)));
        assert!(err.is_err(), "une longueur incoherente doit etre signalee");
    }

    #[test]
    fn samples_can_be_dropped_when_audio_runs_ahead() {
        let mut enc = open();
        {
            let mut sink: PacketOut = Box::new(|_p| Ok(()));
            // Moins d'un bloc AAC : tout reste dans l'accumulateur.
            enc.push(&tone(500, 2), &mut sink).expect("push");
        }
        let before = enc.fifo.size();
        assert!(before > 0);
        let dropped = enc.drop_samples(100).expect("purge");
        assert_eq!(dropped, 100);
        assert_eq!(enc.fifo.size(), before - 100);
        // On ne peut pas jeter plus que ce qui est disponible.
        let rest = enc.drop_samples(10_000).expect("purge");
        assert_eq!(rest as i32, before - 100);
        assert_eq!(enc.fifo.size(), 0);
    }
}
