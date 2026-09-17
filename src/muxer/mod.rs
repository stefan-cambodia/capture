//! Muxage et finalisation du fichier.
//!
//! # Role
//!
//! Le muxer est le seul composant qui touche au fichier. Il vit sur son
//! propre thread et ne fait qu'une chose : recevoir des paquets deja encodes
//! et deja convertis dans la base de temps du flux, puis les ecrire dans
//! l'ordre.
//!
//! # Entrelacement
//!
//! On utilise `av_interleaved_write_frame`, qui met les paquets en attente et
//! les ecrit tries par DTS croissant. C'est indispensable : l'encodeur video
//! et l'encodeur audio produisent leurs paquets sur deux threads, donc dans un
//! ordre arbitraire. Sans entrelacement, le fichier serait illisible en
//! lecture sequentielle.
//!
//! # Finalisation
//!
//! `write_trailer` ecrit la table des echantillons (`moov` pour MP4) : sans
//! lui, le fichier est inexploitable. La sequence d'arret garantit qu'il est
//! appele meme si un encodeur a echoue, et `fsync` s'assure que les donnees
//! ont bien atteint le disque avant que l'on annonce le chemin final.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::Receiver;
use ffmpeg_next as ff;
use ff::{Packet, Rational};

use crate::config::{Config, ContainerFormat};
use crate::error::{RecorderError, Result};
use crate::performance::Stats;

pub mod writer;

use writer::{take_io_error, DiskWriter, SharedIoError};

/// Message adresse au thread de muxage.
pub enum MuxCommand {
    /// Un paquet pret a ecrire : index de flux et base de temps deja poses.
    Packet(Packet),
    /// Le flux video est termine.
    VideoEof,
    /// Le flux audio est termine.
    AudioEof,
}

/// Muxer de sortie.
pub struct Muxer {
    octx: ff::format::context::Output,
    path: PathBuf,
    /// Descripteur jumeau, pour `fsync` une fois ffmpeg referme.
    fsync_handle: Option<File>,
    io_error: SharedIoError,
    stats: Arc<Stats>,
    header_written: bool,
    /// Nombre de flux attendant encore leur fin de flux.
    open_streams: usize,
}

impl Muxer {
    /// Cree le fichier et prepare le conteneur.
    pub fn create(cfg: &Config, stats: Arc<Stats>) -> Result<Self> {
        let path = cfg.output.path.clone();
        let (disk, twin, io_error) =
            DiskWriter::create(&path, Arc::clone(&stats), cfg.video.fps)?;

        let capacity = cfg.output.writer_buffer_kb * 1024;
        let io = ff::format::context::StreamIo::from_write_seek_with_capacity(disk, capacity)
            .map_err(|e| RecorderError::Mux(format!("interface d'ecriture ffmpeg : {e}")))?;

        let name = path.to_string_lossy().to_string();
        let octx = ff::format::output_to_stream(
            io,
            Some(name.as_str()),
            Some(cfg.output.format.muxer_name()),
        )
        .map_err(|e| RecorderError::Mux(format!("conteneur {} refuse : {e}", cfg.output.format.muxer_name())))?;

        Ok(Self {
            octx,
            path,
            fsync_handle: Some(twin),
            io_error,
            stats,
            header_written: false,
            open_streams: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Declare un flux a partir d'un contexte d'encodeur deja ouvert.
    ///
    /// L'encodeur doit etre ouvert : c'est seulement a ce moment que ses
    /// en-tetes (`extradata` : SPS/PPS pour H.264) existent, et ils doivent
    /// figurer dans le conteneur.
    pub fn add_stream(&mut self, ctx: &ff::codec::Context, frame_rate: Option<Rational>) -> Result<usize> {
        if self.header_written {
            return Err(RecorderError::Mux(
                "ajout d'un flux apres l'ecriture de l'en-tete".into(),
            ));
        }
        let mut stream = self
            .octx
            .add_stream_with(ctx)
            .map_err(|e| RecorderError::Mux(format!("ajout d'un flux : {e}")))?;
        // La base de temps demandee n'est qu'un souhait : le conteneur peut
        // en imposer une autre (MP4 preferera souvent 1/15360).
        stream.set_time_base(ctx.time_base());
        if let Some(rate) = frame_rate {
            stream.set_avg_frame_rate(rate);
        }
        let index = stream.index();
        self.open_streams += 1;
        Ok(index)
    }

    /// Ecrit l'en-tete du conteneur. Les bases de temps ne sont definitives
    /// qu'apres cet appel.
    pub fn write_header(&mut self, cfg: &Config) -> Result<()> {
        let mut options = ff::Dictionary::new();
        if cfg.output.format == ContainerFormat::Mp4 && cfg.output.fragmented {
            // Fragmente : chaque groupe d'images est autonome, donc le
            // fichier reste lisible meme apres un arret brutal.
            options.set("movflags", "+frag_keyframe+empty_moov+default_base_moof");
        }
        // Le resultat est evalue avant d'interroger l'erreur d'E/S : cela
        // evite d'emprunter `self` deux fois.
        // `map(|_| ())` libere immediatement le dictionnaire rendu par
        // ffmpeg, qui emprunte le contexte.
        let result = self.octx.write_header_with(options).map(|_| ());
        if let Err(e) = result {
            return Err(self.io_or(RecorderError::Mux(format!("ecriture de l'en-tete : {e}"))));
        }
        self.header_written = true;
        Ok(())
    }

    /// Base de temps definitive d'un flux, a utiliser pour convertir les PTS.
    pub fn stream_time_base(&self, index: usize) -> Result<Rational> {
        self.octx
            .stream(index)
            .map(|s| s.time_base())
            .ok_or_else(|| RecorderError::Mux(format!("flux {index} introuvable")))
    }

    /// Ecrit un paquet, en laissant ffmpeg gerer l'entrelacement.
    pub fn write(&mut self, packet: &Packet) -> Result<()> {
        let result = packet.write_interleaved(&mut self.octx);
        if let Err(e) = result {
            return Err(self.io_or(RecorderError::Mux(format!("ecriture d'un paquet : {e}"))));
        }
        self.stats.packets_muxed(1);
        Ok(())
    }

    /// Boucle du thread de muxage.
    ///
    /// Elle s'arrete quand tous les flux ont signale leur fin, ou quand le
    /// canal se ferme (cas d'un encodeur qui a echoue). Dans les deux cas le
    /// fichier est finalise : un arret, meme provoque par une erreur, ne doit
    /// jamais laisser un fichier corrompu.
    pub fn run(mut self, rx: Receiver<MuxCommand>, capacity: usize) -> Result<PathBuf> {
        let mut first_error: Option<RecorderError> = None;
        let mut remaining = self.open_streams;

        while remaining > 0 {
            let Ok(cmd) = rx.recv() else { break };
            self.stats.set_packet_queue(rx.len(), capacity);
            match cmd {
                MuxCommand::Packet(packet) => {
                    if first_error.is_none() {
                        if let Err(e) = self.write(&packet) {
                            tracing::error!(erreur = %e, "ecriture interrompue");
                            first_error = Some(e);
                        }
                    }
                }
                MuxCommand::VideoEof | MuxCommand::AudioEof => {
                    remaining = remaining.saturating_sub(1);
                }
            }
        }

        let finish = self.finish();
        match (first_error, finish) {
            // L'erreur d'ecriture prime sur celle de finalisation : c'est la
            // cause, pas la consequence.
            (Some(e), _) => Err(e),
            (None, other) => other,
        }
    }

    /// Ecrit la fin du conteneur, vide les tampons et synchronise le disque.
    pub fn finish(mut self) -> Result<PathBuf> {
        if self.header_written {
            let result = self.octx.write_trailer();
            if let Err(e) = result {
                return Err(self.io_or(RecorderError::Mux(format!("finalisation : {e}"))));
            }
        }
        // La destruction du contexte vide le tampon AVIO puis notre writer.
        drop(self.octx);

        if let Some(err) = take_io_error(&self.io_error, &self.path) {
            return Err(err);
        }

        // `fsync` : tant qu'il n'a pas rendu la main, les donnees peuvent
        // n'exister que dans le cache du noyau.
        if let Some(handle) = self.fsync_handle.take() {
            handle
                .sync_all()
                .map_err(|e| RecorderError::from_write(&self.path, e))?;
        }
        Ok(self.path)
    }

    /// Prefere l'erreur systeme reelle au code generique de ffmpeg.
    fn io_or(&self, fallback: RecorderError) -> RecorderError {
        take_io_error(&self.io_error, &self.path).unwrap_or(fallback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HardwarePolicy;
    use crate::encoder::audio::{AudioEncoder, AudioEncoderSpec};
    use crate::encoder::hwdetect;
    use crate::encoder::video::{VideoEncoder, VideoEncoderSpec};
    use crate::encoder::PacketOut;

    fn config(path: PathBuf) -> Config {
        let mut cfg = Config::default();
        cfg.output.path = path;
        cfg.video.encoder = "libx264".into();
        cfg.video.hardware = HardwarePolicy::Off;
        cfg.video.fps = 30;
        cfg
    }

    fn open_encoders(cfg: &Config) -> (VideoEncoder, AudioEncoder) {
        ff::init().ok();
        let v = VideoEncoder::open(&VideoEncoderSpec {
            width: 320,
            height: 240,
            cfg: cfg.video.clone(),
            global_header: true,
            system: hwdetect::detect(),
        })
        .expect("encodeur video");
        let a = AudioEncoder::open(
            &AudioEncoderSpec {
                sample_rate: 48_000,
                channels: 2,
                bitrate: 128_000,
                global_header: true,
            },
            200,
        )
        .expect("encodeur audio");
        (v, a)
    }

    #[test]
    fn a_complete_file_is_produced_and_finalized() {
        let dir = tempfile::tempdir().expect("dossier");
        let path = dir.path().join("out.mp4");
        let cfg = config(path.clone());
        let stats = Stats::shared();
        let (mut venc, mut aenc) = open_encoders(&cfg);

        let mut mux = Muxer::create(&cfg, Arc::clone(&stats)).expect("muxer");
        let vi = mux
            .add_stream(venc.context(), Some(Rational(cfg.video.fps as i32, 1)))
            .expect("flux video");
        let ai = mux.add_stream(aenc.context(), None).expect("flux audio");
        mux.write_header(&cfg).expect("en-tete");
        let vtb = mux.stream_time_base(vi).expect("base video");
        let atb = mux.stream_time_base(ai).expect("base audio");
        let venc_tb = venc.time_base();
        let aenc_tb = aenc.time_base();

        let (tx, rx) = crossbeam_channel::bounded(256);
        let handle = std::thread::spawn(move || mux.run(rx, 256));

        // 1 seconde de video et d'audio.
        {
            let pool = crate::capture::BufferPool::new(2, 320 * 240 * 4);
            let txv = tx.clone();
            let mut sink: PacketOut = Box::new(move |mut p: Packet| {
                p.set_stream(vi);
                p.rescale_ts(venc_tb, vtb);
                txv.send(MuxCommand::Packet(p))
                    .map_err(|_| RecorderError::Mux("file fermee".into()))
            });
            for i in 0..30 {
                let mut buf = pool.acquire();
                for (k, b) in buf.as_mut_slice().iter_mut().enumerate() {
                    *b = ((k + i as usize) % 251) as u8;
                }
                let f = crate::capture::VideoFrame::new(
                    buf,
                    320,
                    240,
                    320 * 4,
                    crate::capture::PixelFormat::Bgrx,
                    0,
                    0,
                );
                venc.encode(Some(&f), i, &mut sink).expect("encode video");
            }
            venc.flush(&mut sink).expect("flush video");
        }
        tx.send(MuxCommand::VideoEof).expect("eof video");

        {
            let txa = tx.clone();
            let mut sink: PacketOut = Box::new(move |mut p: Packet| {
                p.set_stream(ai);
                p.rescale_ts(aenc_tb, atb);
                txa.send(MuxCommand::Packet(p))
                    .map_err(|_| RecorderError::Mux("file fermee".into()))
            });
            let block: Vec<f32> = (0..960).map(|i| (i as f32 * 0.01).sin() * 0.2).collect();
            for _ in 0..100 {
                aenc.push(&block, &mut sink).expect("encode audio");
            }
            aenc.flush(&mut sink).expect("flush audio");
        }
        tx.send(MuxCommand::AudioEof).expect("eof audio");
        drop(tx);

        let out = handle.join().expect("thread muxer").expect("muxage");
        assert_eq!(out, path);

        let size = std::fs::metadata(&path).expect("fichier").len();
        assert!(size > 10_000, "fichier trop petit : {size} octets");
        assert!(stats.snapshot().packets_muxed > 30);

        // Relecture : le fichier doit etre reellement demuxable, avec deux
        // flux et une duree coherente.
        let input = ff::format::input(&path).expect("le fichier doit etre lisible");
        assert_eq!(input.streams().count(), 2, "deux flux attendus");
        let has_video = input
            .streams()
            .any(|s| s.parameters().medium() == ff::media::Type::Video);
        let has_audio = input
            .streams()
            .any(|s| s.parameters().medium() == ff::media::Type::Audio);
        assert!(has_video && has_audio);
        let secs = input.duration() as f64 / ff::ffi::AV_TIME_BASE as f64;
        assert!((0.8..1.4).contains(&secs), "duree relue : {secs} s");
    }

    #[test]
    fn adding_a_stream_after_the_header_is_refused() {
        let dir = tempfile::tempdir().expect("dossier");
        let cfg = config(dir.path().join("late.mp4"));
        let stats = Stats::shared();
        let (venc, _aenc) = open_encoders(&cfg);
        let mut mux = Muxer::create(&cfg, stats).expect("muxer");
        mux.add_stream(venc.context(), None).expect("flux");
        mux.write_header(&cfg).expect("en-tete");
        assert!(mux.add_stream(venc.context(), None).is_err());
    }

    #[test]
    fn an_unwritable_path_fails_at_creation_not_during_recording() {
        let mut cfg = Config::default();
        cfg.output.path = PathBuf::from("/proc/rscap-impossible.mp4");
        let stats = Stats::shared();
        assert!(Muxer::create(&cfg, stats).is_err());
    }

    #[test]
    fn a_fragmented_file_is_also_valid() {
        let dir = tempfile::tempdir().expect("dossier");
        let path = dir.path().join("frag.mp4");
        let mut cfg = config(path.clone());
        cfg.output.fragmented = true;
        let stats = Stats::shared();
        let (mut venc, _a) = open_encoders(&cfg);

        let mut mux = Muxer::create(&cfg, Arc::clone(&stats)).expect("muxer");
        let vi = mux.add_stream(venc.context(), None).expect("flux");
        mux.write_header(&cfg).expect("en-tete");
        let vtb = mux.stream_time_base(vi).expect("base");
        let venc_tb = venc.time_base();

        let pool = crate::capture::BufferPool::new(2, 320 * 240 * 4);
        let mut packets: Vec<Packet> = Vec::new();
        {
            let mut sink: PacketOut = Box::new(|mut p: Packet| {
                p.set_stream(vi);
                p.rescale_ts(venc_tb, vtb);
                packets.push(p);
                Ok(())
            });
            for i in 0..30 {
                let mut buf = pool.acquire();
                buf.as_mut_slice()[0] = i as u8;
                let f = crate::capture::VideoFrame::new(
                    buf,
                    320,
                    240,
                    320 * 4,
                    crate::capture::PixelFormat::Bgrx,
                    0,
                    0,
                );
                venc.encode(Some(&f), i, &mut sink).expect("encode");
            }
            venc.flush(&mut sink).expect("flush");
        }
        for p in &packets {
            mux.write(p).expect("ecriture");
        }
        let out = mux.finish().expect("finalisation");
        assert!(std::fs::metadata(&out).expect("fichier").len() > 1000);
        assert!(ff::format::input(&out).is_ok());
    }
}
