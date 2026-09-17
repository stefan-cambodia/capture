//! Encodeur video : selection materielle, conversion colorimetrique et
//! soumission des images.
//!
//! # Chemin d'une image
//!
//! ```text
//! VideoFrame (BGRx, memoire centrale, pas de ligne du compositeur)
//!   -> AVFrame "vue" : aucun octet copie, on pointe sur le tampon du pool
//!   -> sws_scale_frame : BGRx -> NV12, multithread (libswscale)
//!   -> [materiel a surfaces] av_hwframe_transfer_data : NV12 -> surface GPU
//!   -> avcodec_send_frame
//!   -> avcodec_receive_packet  (0..n paquets)
//! ```
//!
//! # Pourquoi une conversion CPU
//!
//! Le portail nous livre des tampons en memoire centrale : la copie GPU ->CPU
//! a deja eu lieu dans le compositeur, avant que l'on voie l'image. Convertir
//! en NV12 sur le GPU imposerait de remonter le BGRx vers le GPU pour
//! redescendre le NV12, soit deux transferts au lieu d'un.
//!
//! Le chemin reellement sans copie (DMA-BUF exporte par le compositeur,
//! importe directement comme surface VAAPI via `av_hwframe_map`) demande de
//! negocier les modificateurs DRM avec le compositeur. Il n'est pas implemente
//! ici : voir `docs/PIPELINE.md`, section « Chemin zero-copie ».

use std::ffi::CString;
use std::ptr;

use ffmpeg_next as ff;
use ff::ffi;
use ff::format::Pixel;
use ff::{Packet, Rational};

use crate::capture::VideoFrame;
use crate::config::{Pacing, RateControl, VideoCodec, VideoConfig};
use crate::error::{RecorderError, Result};

use super::hwdetect::{self, Acceleration, Candidate, SystemInfo};
use super::{is_again, PacketOut};

/// Parametres de construction de l'encodeur video.
#[derive(Debug, Clone)]
pub struct VideoEncoderSpec {
    pub width: u32,
    pub height: u32,
    pub cfg: VideoConfig,
    /// Le conteneur attend les en-tetes hors flux (MP4, MKV).
    pub global_header: bool,
    pub system: SystemInfo,
}

/// Encodeur video ouvert et pret.
pub struct VideoEncoder {
    encoder: ff::encoder::video::Encoder,
    name: &'static str,
    codec: VideoCodec,
    accel: Acceleration,
    width: u32,
    height: u32,
    time_base: Rational,
    bit_rate: u64,
    sws: Scaler,
    /// Vue sans copie sur le tampon de capture.
    src_view: ff::frame::Video,
    /// Image convertie, reutilisee d'une frame a l'autre. C'est aussi elle que
    /// l'on re-soumet lors d'une duplication : une repetition ne coute donc
    /// aucune conversion.
    staging: ff::frame::Video,
    /// Surface GPU, uniquement pour les encodeurs a `AVHWFramesContext`.
    hw: Option<HwFrames>,
    hw_frame: Option<ff::frame::Video>,
    /// Vrai des qu'une image reelle a ete convertie au moins une fois.
    has_content: bool,
}

// SAFETY: l'encodeur n'est manipule que par le thread d'encodage ; les
// pointeurs bruts qu'il detient (contexte swscale, contextes materiels) ne
// sont jamais partages. `ff::encoder::video::Encoder` est deja `Send`.
unsafe impl Send for VideoEncoder {}

impl VideoEncoder {
    /// Ouvre le premier encodeur utilisable de la liste de candidats.
    ///
    /// L'ouverture reelle est le seul test valable : un encodeur peut etre
    /// compile dans ffmpeg sans que le materiel ou le pilote soit present.
    pub fn open(spec: &VideoEncoderSpec) -> Result<Self> {
        let candidates = hwdetect::candidates(
            spec.cfg.codec,
            spec.cfg.hardware,
            spec.system.vendor(),
            &spec.cfg.encoder,
        );
        if candidates.is_empty() {
            return Err(RecorderError::NoUsableEncoder(
                "aucun candidat pour cette combinaison codec/politique".into(),
            ));
        }

        let mut failures = Vec::new();
        for cand in &candidates {
            match Self::try_open(*cand, spec) {
                Ok(enc) => {
                    if cand.codec != spec.cfg.codec {
                        tracing::warn!(
                            demande = %spec.cfg.codec,
                            retenu = %cand.codec,
                            encodeur = cand.name,
                            "codec demande indisponible : repli"
                        );
                    }
                    tracing::info!(
                        encodeur = enc.name,
                        codec = %enc.codec,
                        materiel = enc.accel.is_hardware(),
                        largeur = enc.width,
                        hauteur = enc.height,
                        debit_kbps = enc.bit_rate / 1000,
                        "encodeur video ouvert"
                    );
                    return Ok(enc);
                }
                Err(e) => {
                    tracing::debug!(encodeur = cand.name, erreur = %e, "candidat ecarte");
                    failures.push(format!("{} ({})", cand.name, e));
                }
            }
        }
        Err(RecorderError::NoUsableEncoder(format!(
            "tous les candidats ont echoue : {}",
            failures.join(", ")
        )))
    }

    fn try_open(cand: Candidate, spec: &VideoEncoderSpec) -> Result<Self> {
        // Les codecs vises exigent des dimensions paires. On rogne d'un pixel
        // plutot que de redimensionner : aucun flou ajoute.
        let width = spec.width & !1;
        let height = spec.height & !1;
        if width < 16 || height < 16 {
            return Err(RecorderError::UnsupportedResolution {
                width: spec.width,
                height: spec.height,
                reason: "moins de 16 pixels apres arrondi",
            });
        }

        let codec = ff::encoder::find_by_name(cand.name).ok_or_else(|| {
            RecorderError::NoUsableEncoder(format!("{} absent de cette build ffmpeg", cand.name))
        })?;

        // Formats logiciels a tenter, dans l'ordre de preference.
        let sw_formats: &[Pixel] = match cand.accel {
            Acceleration::Software => &[Pixel::NV12, Pixel::YUV420P],
            _ => &[Pixel::NV12],
        };

        let mut last: Option<RecorderError> = None;
        for &sw_fmt in sw_formats {
            match Self::build(cand, spec, codec, width, height, sw_fmt) {
                Ok(enc) => return Ok(enc),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            RecorderError::NoUsableEncoder(format!("{} : echec d'ouverture", cand.name))
        }))
    }

    fn build(
        cand: Candidate,
        spec: &VideoEncoderSpec,
        codec: ff::Codec,
        width: u32,
        height: u32,
        sw_fmt: Pixel,
    ) -> Result<Self> {
        let cfg = &spec.cfg;
        let bit_rate = cfg.effective_bitrate(width, height);

        let time_base = match cfg.pacing {
            // Base de temps = 1/fps : le PTS est l'index du slot, donc exact
            // et sans arrondi possible.
            Pacing::Cfr => Rational(1, cfg.fps as i32),
            // En VFR on garde la microseconde : assez fin pour tout ecran,
            // assez grossier pour ne jamais deborder sur une longue session.
            Pacing::Vfr => Rational(1, 1_000_000),
        };

        let mut video = ff::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .map_err(|e| RecorderError::Encode {
                stage: "creation du contexte video",
                source: e,
            })?;

        video.set_width(width);
        video.set_height(height);
        video.set_time_base(time_base);
        video.set_frame_rate(Some(Rational(cfg.fps as i32, 1)));
        video.set_gop(cfg.gop_size());
        video.set_max_b_frames(cfg.b_frames as usize);
        video.set_bit_rate(bit_rate as usize);
        video.set_colorspace(ff::color::Space::BT709);
        video.set_color_range(ff::color::Range::MPEG);
        video.set_color_primaries(ff::color::Primaries::BT709);
        video.set_color_transfer_characteristic(ff::color::TransferCharacteristic::BT709);

        match cfg.rate_control {
            RateControl::Vbr => {
                video.set_max_bit_rate((bit_rate as f64 * 1.5) as usize);
            }
            RateControl::Cbr => {
                video.set_max_bit_rate(bit_rate as usize);
            }
            RateControl::Cq => {
                video.set_global_quality(cfg.quality.cq() as i32);
            }
        }

        if cand.accel == Acceleration::Software {
            // Les encodeurs logiciels sont les seuls a profiter du
            // parallelisme interne de ffmpeg.
            video.set_threading(ff::threading::Config {
                kind: ff::threading::Type::Frame,
                count: spec.system.cpu_threads.clamp(1, 16),
            });
        }

        // Contexte de surfaces GPU, obligatoirement avant `avcodec_open2`.
        let mut hw = None;
        // SAFETY: `video` possede un `AVCodecContext` valide et non encore
        // ouvert ; on n'ecrit que des champs documentes comme modifiables
        // avant ouverture.
        unsafe {
            let ctx = video.as_mut_ptr();
            if spec.global_header {
                (*ctx).flags |= ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32;
            }
            match cand.accel {
                Acceleration::HardwareFrames => {
                    let hw_pix = if cand.name.ends_with("_qsv") {
                        ffi::AVPixelFormat::AV_PIX_FMT_QSV
                    } else {
                        ffi::AVPixelFormat::AV_PIX_FMT_VAAPI
                    };
                    let frames = HwFrames::new(
                        cand.name,
                        spec.cfg.render_node.as_str(),
                        spec.system.render_node(),
                        hw_pix,
                        pixel_to_ffi(sw_fmt),
                        width as i32,
                        height as i32,
                    )?;
                    (*ctx).pix_fmt = hw_pix;
                    (*ctx).hw_frames_ctx = ffi::av_buffer_ref(frames.frames);
                    if (*ctx).hw_frames_ctx.is_null() {
                        return Err(RecorderError::NoUsableEncoder(
                            "reference au contexte de surfaces impossible".into(),
                        ));
                    }
                    hw = Some(frames);
                }
                _ => {
                    (*ctx).pix_fmt = pixel_to_ffi(sw_fmt);
                }
            }
        }

        let options = encoder_options(cand, cfg);
        let encoder = video
            .open_as_with(codec, options)
            .map_err(|e| RecorderError::Encode {
                stage: "ouverture de l'encodeur",
                source: e,
            })?;

        // Image de travail NV12/YUV420P, allouee une fois pour toutes.
        let mut staging = ff::frame::Video::new(sw_fmt, width, height);
        staging.set_color_space(ff::color::Space::BT709);
        staging.set_color_range(ff::color::Range::MPEG);

        let hw_frame = if hw.is_some() {
            Some(ff::frame::Video::empty())
        } else {
            None
        };

        Ok(Self {
            encoder,
            name: cand.name,
            codec: cand.codec,
            accel: cand.accel,
            width,
            height,
            time_base,
            bit_rate,
            sws: Scaler::new(cfg.convert_threads, spec.system.cpu_threads)?,
            src_view: ff::frame::Video::empty(),
            staging,
            hw,
            hw_frame,
            has_content: false,
        })
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    pub fn is_hardware(&self) -> bool {
        self.accel.is_hardware()
    }

    pub fn time_base(&self) -> Rational {
        self.time_base
    }

    pub fn bit_rate(&self) -> u64 {
        self.bit_rate
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Contexte ffmpeg, pour declarer le flux au muxer.
    pub fn context(&self) -> &ff::codec::Context {
        &self.encoder
    }

    /// Encode une image.
    ///
    /// `src == None` signifie « repeter la derniere image » : aucune
    /// conversion n'est refaite, on resoumet l'image de travail avec un
    /// nouveau PTS. C'est ce qui rend une duplication quasi gratuite.
    pub fn encode(&mut self, src: Option<&VideoFrame>, pts: i64, out: &mut PacketOut<'_>) -> Result<()> {
        if let Some(frame) = src {
            self.convert(frame)?;
            self.has_content = true;
        } else if !self.has_content {
            // Rien a repeter : la capture n'a encore rien fourni.
            return Ok(());
        }

        let send_result = match self.hw.as_ref() {
            Some(hw) => {
                let hw_frame = self
                    .hw_frame
                    .as_mut()
                    .ok_or_else(|| RecorderError::Mux("surface GPU absente".into()))?;
                // SAFETY: `hw_frame` est un AVFrame valide ; on le rend a
                // l'etat vierge avant d'en demander un nouveau au pool GPU.
                unsafe {
                    ffi::av_frame_unref(hw_frame.as_mut_ptr());
                    let rc = ffi::av_hwframe_get_buffer(hw.frames, hw_frame.as_mut_ptr(), 0);
                    if rc < 0 {
                        return Err(RecorderError::Encode {
                            stage: "allocation d'une surface GPU",
                            source: ff::Error::from(rc),
                        });
                    }
                    let rc = ffi::av_hwframe_transfer_data(
                        hw_frame.as_mut_ptr(),
                        self.staging.as_ptr(),
                        0,
                    );
                    if rc < 0 {
                        return Err(RecorderError::Encode {
                            stage: "televersement vers le GPU",
                            source: ff::Error::from(rc),
                        });
                    }
                    (*hw_frame.as_mut_ptr()).pts = pts;
                }
                self.encoder.send_frame(hw_frame)
            }
            None => {
                // SAFETY: AVFrame valide detenu par `self`.
                unsafe {
                    (*self.staging.as_mut_ptr()).pts = pts;
                }
                self.encoder.send_frame(&self.staging)
            }
        };

        send_result.map_err(|e| RecorderError::Encode {
            stage: "soumission de l'image",
            source: e,
        })?;
        self.drain(out)
    }

    /// Termine l'encodage et rend les images encore retenues par l'encodeur.
    pub fn flush(&mut self, out: &mut PacketOut<'_>) -> Result<()> {
        self.encoder.send_eof().map_err(|e| RecorderError::Encode {
            stage: "fin de flux video",
            source: e,
        })?;
        self.drain(out)
    }

    fn drain(&mut self, out: &mut PacketOut<'_>) -> Result<()> {
        loop {
            let mut packet = Packet::empty();
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => out(packet)?,
                Err(e) if is_again(&e) => return Ok(()),
                Err(e) => {
                    return Err(RecorderError::Encode {
                        stage: "recuperation d'un paquet video",
                        source: e,
                    })
                }
            }
        }
    }

    /// BGRx -> NV12 sans copie intermediaire.
    fn convert(&mut self, frame: &VideoFrame) -> Result<()> {
        let pix = frame.format().to_ffmpeg();
        // SAFETY: on construit une vue sur le tampon de capture, qui reste
        // vivant pendant tout l'appel (`frame` est emprunte). Les champs
        // `buf[]` restent nuls : ffmpeg ne liberera donc jamais ces octets.
        unsafe {
            let p = self.src_view.as_mut_ptr();
            ffi::av_frame_unref(p);
            (*p).format = pixel_to_ffi(pix) as i32;
            (*p).width = self.width as i32;
            (*p).height = self.height as i32;
            (*p).data[0] = frame.data().as_ptr() as *mut u8;
            (*p).linesize[0] = frame.stride() as i32;
            // Le BGRx du compositeur est en plage complete et en primaires
            // sRGB ; c'est swscale qui fera la reduction vers la plage TV
            // demandee par l'image de destination.
            (*p).color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
            (*p).colorspace = ffi::AVColorSpace::AVCOL_SPC_RGB;
        }
        self.sws.convert(&self.src_view, &mut self.staging)
    }
}

/// Options privees, specifiques a chaque famille d'encodeur.
fn encoder_options(cand: Candidate, cfg: &VideoConfig) -> ff::Dictionary<'static> {
    let mut opts = ff::Dictionary::new();
    let preset = cfg.preset.as_str();
    let cq = cfg.quality.cq().to_string();

    if cand.name.ends_with("_vaapi") {
        // VAAPI attend un mode explicite ; sans cela le pilote choisit CQP
        // et ignore le debit demande.
        let rc = match cfg.rate_control {
            RateControl::Vbr => "VBR",
            RateControl::Cbr => "CBR",
            RateControl::Cq => "CQP",
        };
        opts.set("rc_mode", rc);
        if cfg.rate_control == RateControl::Cq {
            opts.set("qp", &cq);
        }
        // Profondeur de reference : 1 = pas de trame B, latence minimale.
        if cfg.b_frames == 0 {
            opts.set("bf", "0");
        }
    } else if cand.name.ends_with("_nvenc") {
        // p1 = le plus rapide, p7 = le plus lent. p4 est le compromis retenu
        // par defaut pour tenir 60 fps en 1440p sans saturer l'encodeur.
        opts.set("preset", if preset.is_empty() { "p4" } else { preset });
        opts.set("tune", "hq");
        opts.set(
            "rc",
            match cfg.rate_control {
                RateControl::Vbr => "vbr",
                RateControl::Cbr => "cbr",
                RateControl::Cq => "constqp",
            },
        );
        if cfg.rate_control == RateControl::Cq {
            opts.set("cq", &cq);
        }
        // L'encodeur NVENC peut lire directement la memoire centrale.
        opts.set("delay", "0");
    } else if cand.name.ends_with("_qsv") {
        opts.set(
            "preset",
            if preset.is_empty() { "veryfast" } else { preset },
        );
        if cfg.rate_control == RateControl::Cq {
            opts.set("global_quality", &cq);
        }
    } else if cand.name.ends_with("_amf") {
        opts.set("usage", "lowlatency_high_quality");
        opts.set(
            "rc",
            match cfg.rate_control {
                RateControl::Vbr => "vbr_peak",
                RateControl::Cbr => "cbr",
                RateControl::Cq => "cqp",
            },
        );
        if cfg.rate_control == RateControl::Cq {
            opts.set("qp_i", &cq);
            opts.set("qp_p", &cq);
        }
    } else if cand.name == "libx264" || cand.name == "libx265" {
        // `veryfast` est le point ou x264 tient encore 1440p60 sur un CPU de
        // portable tout en restant nettement meilleur que `ultrafast`.
        opts.set(
            "preset",
            if preset.is_empty() { "veryfast" } else { preset },
        );
        if cfg.rate_control == RateControl::Cq {
            opts.set("crf", &cq);
        }
        // Contenu synthetique : beaucoup d'aplats et de texte net.
        opts.set("tune", if cand.name == "libx264" { "stillimage" } else { "grain" });
    } else if cand.name == "libsvtav1" {
        opts.set("preset", if preset.is_empty() { "9" } else { preset });
        if cfg.rate_control == RateControl::Cq {
            opts.set("crf", &cq);
        }
    }
    opts
}

fn pixel_to_ffi(p: Pixel) -> ffi::AVPixelFormat {
    // `Pixel` est un enum miroir de `AVPixelFormat` : la conversion est
    // fournie par ffmpeg-next.
    p.into()
}

// ---------------------------------------------------------------------------
// Contexte de surfaces GPU
// ---------------------------------------------------------------------------

/// `AVHWDeviceContext` + `AVHWFramesContext`, pour VAAPI et QSV.
struct HwFrames {
    device: *mut ffi::AVBufferRef,
    frames: *mut ffi::AVBufferRef,
}

impl HwFrames {
    fn new(
        encoder_name: &str,
        configured_node: &str,
        detected_node: Option<&std::path::Path>,
        hw_pix: ffi::AVPixelFormat,
        sw_pix: ffi::AVPixelFormat,
        width: i32,
        height: i32,
    ) -> Result<Self> {
        let kind = if encoder_name.ends_with("_qsv") {
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_QSV
        } else {
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI
        };

        let node: Option<String> = if !configured_node.is_empty() {
            Some(configured_node.to_owned())
        } else {
            detected_node.map(|p| p.display().to_string())
        };
        let node_c = node
            .as_deref()
            .map(CString::new)
            .transpose()
            .map_err(|_| RecorderError::NoUsableEncoder("noeud de rendu invalide".into()))?;

        let mut device: *mut ffi::AVBufferRef = ptr::null_mut();
        // SAFETY: appel ffmpeg standard ; `device` recoit une reference dont
        // nous devenons proprietaire.
        let rc = unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut device,
                kind,
                node_c.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
                ptr::null_mut(),
                0,
            )
        };
        if rc < 0 || device.is_null() {
            return Err(RecorderError::NoUsableEncoder(format!(
                "peripherique {} indisponible{}",
                if kind == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_QSV {
                    "QSV"
                } else {
                    "VAAPI"
                },
                node.map(|n| format!(" ({n})")).unwrap_or_default()
            )));
        }

        // SAFETY: `device` est une reference valide rendue ci-dessus.
        let frames = unsafe { ffi::av_hwframe_ctx_alloc(device) };
        if frames.is_null() {
            // SAFETY: `device` est valide et nous en sommes proprietaire.
            unsafe { ffi::av_buffer_unref(&mut device) };
            return Err(RecorderError::NoUsableEncoder(
                "allocation du contexte de surfaces impossible".into(),
            ));
        }

        // SAFETY: `frames->data` pointe sur un `AVHWFramesContext` fraichement
        // alloue, dont ces champs doivent etre renseignes avant l'init.
        unsafe {
            let ctx = (*frames).data as *mut ffi::AVHWFramesContext;
            (*ctx).format = hw_pix;
            (*ctx).sw_format = sw_pix;
            (*ctx).width = width;
            (*ctx).height = height;
            // Assez de surfaces pour couvrir la file d'encodage et les images
            // de reference, sans immobiliser trop de memoire video.
            (*ctx).initial_pool_size = 24;
        }

        // SAFETY: le contexte vient d'etre renseigne comme documente.
        let rc = unsafe { ffi::av_hwframe_ctx_init(frames) };
        if rc < 0 {
            let mut frames = frames;
            // SAFETY: les deux references sont valides et nous appartiennent.
            unsafe {
                ffi::av_buffer_unref(&mut frames);
                ffi::av_buffer_unref(&mut device);
            }
            return Err(RecorderError::NoUsableEncoder(format!(
                "initialisation des surfaces {width}x{height} refusee par le pilote"
            )));
        }

        Ok(Self { device, frames })
    }
}

impl Drop for HwFrames {
    fn drop(&mut self) {
        // SAFETY: les deux references nous appartiennent et ne sont liberees
        // qu'ici, une seule fois.
        unsafe {
            if !self.frames.is_null() {
                ffi::av_buffer_unref(&mut self.frames);
            }
            if !self.device.is_null() {
                ffi::av_buffer_unref(&mut self.device);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion colorimetrique
// ---------------------------------------------------------------------------

/// Enveloppe autour de `SwsContext` en mode dynamique.
///
/// En mode dynamique, `sws_scale_frame` lit toutes les proprietes (format,
/// taille, plage, espace colorimetrique) depuis les `AVFrame` : un seul
/// contexte suffit meme si la geometrie change, et `threads` active le
/// decoupage en bandes de libswscale.
struct Scaler {
    ctx: *mut ffi::SwsContext,
}

impl Scaler {
    fn new(configured: u32, cpu_threads: usize) -> Result<Self> {
        // SAFETY: allocation ffmpeg simple.
        let ctx = unsafe { ffi::sws_alloc_context() };
        if ctx.is_null() {
            return Err(RecorderError::NoUsableEncoder(
                "contexte de conversion indisponible".into(),
            ));
        }
        let threads = if configured > 0 {
            configured as i32
        } else {
            // La conversion n'est pas le goulot d'etranglement : deux fils
            // suffisent en 1440p, au-dela le gain est nul et on vole du temps
            // CPU a l'encodeur logiciel.
            (cpu_threads / 4).clamp(1, 4) as i32
        };
        // SAFETY: `ctx` vient d'etre alloue et n'est pas encore initialise.
        unsafe {
            (*ctx).threads = threads;
            (*ctx).flags = ffi::SwsFlags::SWS_BILINEAR as libc::c_uint;
        }
        Ok(Self { ctx })
    }

    fn convert(&mut self, src: &ff::frame::Video, dst: &mut ff::frame::Video) -> Result<()> {
        // SAFETY: les deux AVFrame sont valides ; `sws_scale_frame` en mode
        // dynamique n'exige aucune initialisation prealable du contexte.
        let rc = unsafe { ffi::sws_scale_frame(self.ctx, dst.as_mut_ptr(), src.as_ptr()) };
        if rc < 0 {
            return Err(RecorderError::Encode {
                stage: "conversion colorimetrique",
                source: ff::Error::from(rc),
            });
        }
        Ok(())
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        // SAFETY: le contexte nous appartient et n'est libere qu'ici.
        unsafe { ffi::sws_free_context(&mut self.ctx) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{BufferPool, PixelFormat};
    use crate::config::{HardwarePolicy, Quality};

    fn spec(w: u32, h: u32, policy: HardwarePolicy) -> VideoEncoderSpec {
        let cfg = VideoConfig {
            hardware: policy,
            quality: Quality::Medium,
            fps: 60,
            ..VideoConfig::default()
        };
        VideoEncoderSpec {
            width: w,
            height: h,
            cfg,
            global_header: true,
            system: hwdetect::detect(),
        }
    }

    fn test_frame(pool: &std::sync::Arc<BufferPool>, w: u32, h: u32) -> VideoFrame {
        let mut buf = pool.acquire();
        for (i, b) in buf.as_mut_slice().iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        VideoFrame::new(buf, w, h, w * 4, PixelFormat::Bgrx, 0, 0)
    }

    #[test]
    fn software_encoder_opens_and_produces_packets() {
        ff::init().ok();
        let mut s = spec(320, 240, HardwarePolicy::Off);
        s.cfg.encoder = "libx264".into();
        let mut enc = VideoEncoder::open(&s).expect("libx264 doit etre disponible");
        assert!(!enc.is_hardware());
        assert_eq!(enc.time_base(), Rational(1, 60));

        let pool = BufferPool::new(2, 320 * 240 * 4);
        let mut packets = 0usize;
        let mut sink: PacketOut = Box::new(|_p| {
            packets += 1;
            Ok(())
        });
        for i in 0..30 {
            let f = test_frame(&pool, 320, 240);
            enc.encode(Some(&f), i, &mut sink).expect("encodage");
        }
        enc.flush(&mut sink).expect("vidange");
        drop(sink);
        assert!(packets >= 30, "paquets produits : {packets}");
    }

    #[test]
    fn odd_dimensions_are_rounded_down_to_even() {
        ff::init().ok();
        let mut s = spec(321, 241, HardwarePolicy::Off);
        s.cfg.encoder = "libx264".into();
        let enc = VideoEncoder::open(&s).expect("ouverture");
        assert_eq!((enc.width(), enc.height()), (320, 240));
    }

    #[test]
    fn a_resolution_below_the_minimum_is_rejected() {
        ff::init().ok();
        let mut s = spec(8, 8, HardwarePolicy::Off);
        s.cfg.encoder = "libx264".into();
        assert!(VideoEncoder::open(&s).is_err());
    }

    #[test]
    fn duplicating_a_frame_needs_no_source() {
        ff::init().ok();
        let mut s = spec(320, 240, HardwarePolicy::Off);
        s.cfg.encoder = "libx264".into();
        let mut enc = VideoEncoder::open(&s).expect("ouverture");
        let pool = BufferPool::new(2, 320 * 240 * 4);

        let mut count = 0usize;
        let mut sink: PacketOut = Box::new(|_p| {
            count += 1;
            Ok(())
        });
        // Une duplication avant toute image reelle ne produit rien et
        // n'echoue pas.
        enc.encode(None, 0, &mut sink).expect("duplication a vide");
        let f = test_frame(&pool, 320, 240);
        enc.encode(Some(&f), 0, &mut sink).expect("image reelle");
        // Puis des duplications sans fournir de source.
        for i in 1..10 {
            enc.encode(None, i, &mut sink).expect("duplication");
        }
        enc.flush(&mut sink).expect("vidange");
        drop(sink);
        assert!(count >= 10, "paquets : {count}");
    }

    #[test]
    fn vfr_pacing_uses_a_microsecond_time_base() {
        ff::init().ok();
        let mut s = spec(320, 240, HardwarePolicy::Off);
        s.cfg.encoder = "libx264".into();
        s.cfg.pacing = Pacing::Vfr;
        let enc = VideoEncoder::open(&s).expect("ouverture");
        assert_eq!(enc.time_base(), Rational(1, 1_000_000));
    }

    #[test]
    fn an_unknown_encoder_name_fails_cleanly() {
        ff::init().ok();
        let mut s = spec(320, 240, HardwarePolicy::Auto);
        s.cfg.encoder = "encodeur_inexistant".into();
        let err = VideoEncoder::open(&s);
        assert!(matches!(err, Err(RecorderError::NoUsableEncoder(_))));
    }

    #[test]
    fn hardware_encoder_opens_when_the_gpu_supports_it() {
        ff::init().ok();
        let s = spec(640, 480, HardwarePolicy::Force);
        match VideoEncoder::open(&s) {
            Ok(mut enc) => {
                assert!(enc.is_hardware());
                let pool = BufferPool::new(2, 640 * 480 * 4);
                let mut packets = 0usize;
                let mut sink: PacketOut = Box::new(|_p| {
                    packets += 1;
                    Ok(())
                });
                for i in 0..20 {
                    let f = test_frame(&pool, 640, 480);
                    enc.encode(Some(&f), i, &mut sink).expect("encodage materiel");
                }
                enc.flush(&mut sink).expect("vidange");
                drop(sink);
                assert!(packets > 0);
            }
            Err(e) => {
                // Machine sans GPU utilisable : le test ne doit pas echouer,
                // mais l'erreur doit etre explicite.
                eprintln!("encodage materiel indisponible ici : {e}");
            }
        }
    }
}
