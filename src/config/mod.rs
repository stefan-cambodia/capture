//! Configuration de l'enregistreur : valeurs par defaut, chargement TOML,
//! validation.
//!
//! Toute valeur est validee avant que la moindre ressource ne soit ouverte :
//! il vaut mieux refuser une configuration incoherente au demarrage que
//! decouvrir le probleme au milieu d'un enregistrement.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{RecorderError, Result};

/// Codec video demande.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    #[serde(alias = "h265", alias = "h.265")]
    Hevc,
    Av1,
}

impl VideoCodec {
    pub fn as_str(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::Hevc => "hevc",
            VideoCodec::Av1 => "av1",
        }
    }

    pub fn ffmpeg_id(self) -> ffmpeg_next::codec::Id {
        match self {
            VideoCodec::H264 => ffmpeg_next::codec::Id::H264,
            VideoCodec::Hevc => ffmpeg_next::codec::Id::HEVC,
            VideoCodec::Av1 => ffmpeg_next::codec::Id::AV1,
        }
    }
}

impl fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Niveau de qualite. Il pilote le debit cible quand `bitrate` vaut 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Low,
    Medium,
    High,
    #[serde(alias = "veryhigh", alias = "very_high")]
    VeryHigh,
}

impl Quality {
    /// Bits par pixel et par seconde, calibre pour une capture d'ecran
    /// (contenu majoritairement statique, texte net a preserver).
    fn bits_per_pixel_per_second(self) -> f64 {
        match self {
            Quality::Low => 0.035,
            Quality::Medium => 0.07,
            Quality::High => 0.12,
            Quality::VeryHigh => 0.20,
        }
    }

    /// Quantificateur constant equivalent, pour les modes qualite.
    pub fn cq(self) -> u32 {
        match self {
            Quality::Low => 32,
            Quality::Medium => 27,
            Quality::High => 23,
            Quality::VeryHigh => 19,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Quality::Low => "low",
            Quality::Medium => "medium",
            Quality::High => "high",
            Quality::VeryHigh => "very_high",
        }
    }
}

/// Politique d'encodage materiel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HardwarePolicy {
    /// Materiel si disponible, sinon repli logiciel. Defaut.
    Auto,
    /// Materiel obligatoire : echec au demarrage si indisponible.
    Force,
    /// Logiciel uniquement.
    Off,
}

/// Strategie de controle de debit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateControl {
    /// Debit variable borne : le meilleur compromis pour une capture d'ecran.
    Vbr,
    /// Debit constant : utile pour le streaming, gaspilleur sur un ecran fixe.
    Cbr,
    /// Qualite constante : taille imprevisible mais qualite homogene.
    Cq,
}

/// Cadencement du flux video de sortie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pacing {
    /// Grille reguliere `1/fps`. Compatible avec tous les lecteurs et
    /// logiciels de montage. Les trous sont combles par repetition d'image
    /// (comptee et signalee).
    Cfr,
    /// Chaque frame conserve son timestamp de capture. Fichier plus petit sur
    /// un ecran statique, mais certains editeurs le gerent mal.
    Vfr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VideoConfig {
    /// Images par seconde visees.
    pub fps: u32,
    pub codec: VideoCodec,
    pub quality: Quality,
    /// Debit cible en bits/s. `0` = calcule depuis `quality` et la resolution.
    pub bitrate: u64,
    pub rate_control: RateControl,
    /// Intervalle entre images cles, en secondes.
    pub keyframe_interval_secs: f64,
    /// Nombre de trames B. `0` par defaut : elles ajoutent de la latence et
    /// n'apportent presque rien sur une capture d'ecran.
    pub b_frames: u32,
    /// Preset de l'encodeur (`""` = choix automatique selon l'encodeur).
    pub preset: String,
    /// Politique materielle.
    pub hardware: HardwarePolicy,
    /// Force un encodeur precis (ex. `h264_vaapi`). `""` = selection auto.
    pub encoder: String,
    /// Noeud DRM utilise pour VAAPI/QSV.
    pub render_node: String,
    pub pacing: Pacing,
    /// Marge d'absorption du jitter de capture, en millisecondes.
    ///
    /// Un slot n'est emis que `lookahead_ms` apres son echeance theorique :
    /// c'est le temps laisse a l'image pour arriver. La valeur doit depasser
    /// la latence de livraison de la source (visible en p99 de « capture »
    /// dans `--benchmark`), sinon des images fraiches arrivent trop tard et
    /// se retrouvent re-calees d'un slot. Une periode d'image complete est un
    /// bon defaut pour un enregistreur, ou la latence n'a aucune importance.
    pub lookahead_ms: u64,
    /// Threads de conversion colorimetrique. `0` = automatique.
    pub convert_threads: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            fps: 60,
            codec: VideoCodec::H264,
            quality: Quality::High,
            bitrate: 0,
            rate_control: RateControl::Vbr,
            keyframe_interval_secs: 2.0,
            b_frames: 0,
            preset: String::new(),
            hardware: HardwarePolicy::Auto,
            encoder: String::new(),
            render_node: String::new(),
            pacing: Pacing::Cfr,
            lookahead_ms: 16,
            convert_threads: 0,
        }
    }
}

impl VideoConfig {
    /// Debit effectif pour une resolution donnee.
    pub fn effective_bitrate(&self, width: u32, height: u32) -> u64 {
        if self.bitrate > 0 {
            return self.bitrate;
        }
        let pixels = width as f64 * height as f64;
        let bps = pixels * self.fps as f64 * self.quality.bits_per_pixel_per_second();
        // Les codecs recents font aussi bien avec moins de debit.
        let factor = match self.codec {
            VideoCodec::H264 => 1.0,
            VideoCodec::Hevc => 0.7,
            VideoCodec::Av1 => 0.6,
        };
        ((bps * factor) as u64).clamp(1_000_000, 200_000_000)
    }

    pub fn gop_size(&self) -> u32 {
        ((self.keyframe_interval_secs * self.fps as f64).round() as u32).max(1)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub enabled: bool,
    pub sample_rate: u32,
    pub channels: u16,
    /// Debit AAC en bits/s.
    pub bitrate: u64,
    /// Source systeme. `""` = moniteur de la sortie par defaut.
    pub device: String,
    /// Taille d'un fragment de capture, en millisecondes. Plus petit = moins
    /// de latence mais plus de reveils.
    pub fragment_ms: u32,
    /// Active l'asservissement de l'horloge audio sur l'horloge monotone.
    pub drift_correction: bool,
    /// Au-dela de cet ecart, on considere qu'il y a eu un trou (peripherique
    /// suspendu) et on insere du silence au lieu de compenser.
    pub max_drift_ms: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_rate: 48_000,
            channels: 2,
            bitrate: 192_000,
            device: String::new(),
            fragment_ms: 10,
            drift_correction: true,
            max_drift_ms: 200,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerFormat {
    Mp4,
    Mkv,
}

impl ContainerFormat {
    pub fn muxer_name(self) -> &'static str {
        match self {
            ContainerFormat::Mp4 => "mp4",
            ContainerFormat::Mkv => "matroska",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ContainerFormat::Mp4 => "mp4",
            ContainerFormat::Mkv => "mkv",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    pub path: PathBuf,
    pub format: ContainerFormat,
    /// MP4 fragmente : le fichier reste lisible meme apres un arret brutal,
    /// au prix d'une compatibilite moindre avec certains editeurs.
    pub fragmented: bool,
    /// Taille du tampon d'ecriture, en kilo-octets. Des ecritures larges et
    /// peu frequentes valent mieux que beaucoup de petites.
    pub writer_buffer_kb: usize,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("recording.mp4"),
            format: ContainerFormat::Mp4,
            fragmented: false,
            writer_buffer_kb: 4096,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineConfig {
    /// Frames video en vol entre capture et encodage.
    pub video_queue: usize,
    /// Fragments audio en vol entre capture et encodage.
    pub audio_queue: usize,
    /// Paquets encodes en vol entre encodeurs et muxer.
    pub packet_queue: usize,
    /// Nombre de tampons video preallouees dans le pool.
    pub frame_pool: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            // ~130 ms a 60 fps : assez pour absorber une rafale, assez court
            // pour que la latence reste bornee.
            video_queue: 8,
            // ~640 ms a 10 ms par fragment.
            audio_queue: 64,
            packet_queue: 256,
            frame_pool: 12,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub video: VideoConfig,
    pub audio: AudioConfig,
    pub output: OutputConfig,
    pub pipeline: PipelineConfig,
}

impl Config {
    /// Charge un fichier TOML puis valide.
    pub fn from_toml_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            RecorderError::Config(format!("lecture de {} impossible : {e}", path.display()))
        })?;
        Self::from_toml_str(&text)
    }

    pub fn from_toml_str(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text)
            .map_err(|e| RecorderError::Config(format!("TOML invalide : {e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|e| RecorderError::Config(format!("serialisation impossible : {e}")))
    }

    /// Verifie la coherence de toutes les valeurs.
    pub fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(RecorderError::Config(m));

        // --- video ---
        if !(1..=480).contains(&self.video.fps) {
            return bad(format!(
                "video.fps = {} hors bornes (1..=480)",
                self.video.fps
            ));
        }
        if self.video.fps < 30 {
            return bad(format!(
                "video.fps = {} : l'objectif minimum du projet est 30 FPS",
                self.video.fps
            ));
        }
        if self.video.bitrate != 0 && !(100_000..=500_000_000).contains(&self.video.bitrate) {
            return bad(format!(
                "video.bitrate = {} hors bornes (100 kb/s .. 500 Mb/s, ou 0 pour automatique)",
                self.video.bitrate
            ));
        }
        if !(0.1..=60.0).contains(&self.video.keyframe_interval_secs) {
            return bad(format!(
                "video.keyframe_interval_secs = {} hors bornes (0.1 .. 60)",
                self.video.keyframe_interval_secs
            ));
        }
        if self.video.b_frames > 16 {
            return bad(format!(
                "video.b_frames = {} : maximum 16",
                self.video.b_frames
            ));
        }
        if self.video.lookahead_ms > 1000 {
            return bad(format!(
                "video.lookahead_ms = {} : au-dela de 1000 ms la latence n'a plus de sens",
                self.video.lookahead_ms
            ));
        }
        if self.video.convert_threads > 64 {
            return bad("video.convert_threads : maximum 64".into());
        }
        if self.video.hardware == HardwarePolicy::Off && !self.video.encoder.is_empty() {
            let is_hw = ["vaapi", "qsv", "nvenc", "amf", "videotoolbox", "mf"]
                .iter()
                .any(|s| self.video.encoder.contains(s));
            if is_hw {
                return bad(format!(
                    "video.encoder = \"{}\" est un encodeur materiel alors que video.hardware = \"off\"",
                    self.video.encoder
                ));
            }
        }

        // --- audio ---
        if self.audio.enabled {
            const RATES: [u32; 6] = [8_000, 16_000, 22_050, 32_000, 44_100, 48_000];
            if !RATES.contains(&self.audio.sample_rate) {
                return bad(format!(
                    "audio.sample_rate = {} non supporte (valeurs : {RATES:?})",
                    self.audio.sample_rate
                ));
            }
            if !(1..=2).contains(&self.audio.channels) {
                return bad(format!(
                    "audio.channels = {} : seuls le mono et le stereo sont geres",
                    self.audio.channels
                ));
            }
            if !(32_000..=512_000).contains(&self.audio.bitrate) {
                return bad(format!(
                    "audio.bitrate = {} hors bornes (32 .. 512 kb/s)",
                    self.audio.bitrate
                ));
            }
            if !(1..=100).contains(&self.audio.fragment_ms) {
                return bad(format!(
                    "audio.fragment_ms = {} hors bornes (1 .. 100)",
                    self.audio.fragment_ms
                ));
            }
            if !(10..=5000).contains(&self.audio.max_drift_ms) {
                return bad(format!(
                    "audio.max_drift_ms = {} hors bornes (10 .. 5000)",
                    self.audio.max_drift_ms
                ));
            }
        }

        // --- sortie ---
        if self.output.path.as_os_str().is_empty() {
            return bad("output.path est vide".into());
        }
        if let Some(parent) = self.output.path.parent() {
            if !parent.as_os_str().is_empty() && !parent.is_dir() {
                return bad(format!(
                    "le dossier de sortie {} n'existe pas",
                    parent.display()
                ));
            }
        }
        if !(64..=262_144).contains(&self.output.writer_buffer_kb) {
            return bad(format!(
                "output.writer_buffer_kb = {} hors bornes (64 Kio .. 256 Mio)",
                self.output.writer_buffer_kb
            ));
        }
        if self.output.format == ContainerFormat::Mp4
            && self.video.codec == VideoCodec::Av1
            && !self.output.fragmented
        {
            // Autorise, mais on prefere le signaler tot.
            tracing::warn!(
                "AV1 dans un MP4 non fragmente : verifiez la compatibilite de votre lecteur"
            );
        }

        // --- pipeline ---
        if !(2..=240).contains(&self.pipeline.video_queue) {
            return bad(format!(
                "pipeline.video_queue = {} hors bornes (2 .. 240)",
                self.pipeline.video_queue
            ));
        }
        if !(2..=4096).contains(&self.pipeline.audio_queue) {
            return bad(format!(
                "pipeline.audio_queue = {} hors bornes (2 .. 4096)",
                self.pipeline.audio_queue
            ));
        }
        if !(8..=8192).contains(&self.pipeline.packet_queue) {
            return bad(format!(
                "pipeline.packet_queue = {} hors bornes (8 .. 8192)",
                self.pipeline.packet_queue
            ));
        }
        if self.pipeline.frame_pool < self.pipeline.video_queue + 2 {
            return bad(format!(
                "pipeline.frame_pool ({}) doit valoir au moins video_queue + 2 ({})",
                self.pipeline.frame_pool,
                self.pipeline.video_queue + 2
            ));
        }
        Ok(())
    }

    /// Aligne l'extension du fichier de sortie sur le conteneur choisi.
    pub fn normalize_output_extension(&mut self) {
        let want = self.output.format.extension();
        let ok = self
            .output
            .path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(want));
        if !ok {
            self.output.path.set_extension(want);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn default_targets_60_fps_h264_mp4() {
        let c = Config::default();
        assert_eq!(c.video.fps, 60);
        assert_eq!(c.video.codec, VideoCodec::H264);
        assert_eq!(c.output.format, ContainerFormat::Mp4);
        assert_eq!(c.audio.sample_rate, 48_000);
        assert_eq!(c.audio.channels, 2);
    }

    #[test]
    fn roundtrip_through_toml() {
        let c = Config::default();
        let text = c.to_toml().expect("serialisation");
        let back = Config::from_toml_str(&text).expect("deserialisation");
        assert_eq!(c, back);
    }

    #[test]
    fn example_config_from_the_readme_parses() {
        let text = r#"
            [video]
            fps = 60
            codec = "h264"
            quality = "high"
            bitrate = 35000000
            hardware = "auto"

            [audio]
            sample_rate = 48000
            channels = 2

            [output]
            path = "out.mp4"
            format = "mp4"
        "#;
        let c = Config::from_toml_str(text).expect("config valide");
        assert_eq!(c.video.bitrate, 35_000_000);
        assert_eq!(c.output.path, PathBuf::from("out.mp4"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = Config::from_toml_str("[video]\nfpss = 60\n");
        assert!(err.is_err(), "une cle inconnue doit etre signalee");
    }

    #[test]
    fn fps_below_the_project_minimum_is_rejected() {
        let mut c = Config::default();
        c.video.fps = 24;
        assert!(c.validate().is_err());
    }

    #[test]
    fn absurd_values_are_rejected() {
        let mut c = Config::default();
        c.video.fps = 100_000;
        assert!(c.validate().is_err());

        let mut c = Config::default();
        c.audio.sample_rate = 12_345;
        assert!(c.validate().is_err());

        let mut c = Config::default();
        c.audio.channels = 8;
        assert!(c.validate().is_err());

        let mut c = Config::default();
        c.video.bitrate = 42;
        assert!(c.validate().is_err());
    }

    #[test]
    fn frame_pool_must_cover_the_queue() {
        let mut c = Config::default();
        c.pipeline.video_queue = 32;
        c.pipeline.frame_pool = 8;
        assert!(c.validate().is_err());
    }

    #[test]
    fn software_policy_rejects_a_hardware_encoder() {
        let mut c = Config::default();
        c.video.hardware = HardwarePolicy::Off;
        c.video.encoder = "h264_vaapi".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn automatic_bitrate_scales_with_resolution_and_fps() {
        let mut c = VideoConfig {
            bitrate: 0,
            ..VideoConfig::default()
        };
        let b1080 = c.effective_bitrate(1920, 1080);
        let b1440 = c.effective_bitrate(2560, 1440);
        assert!(b1440 > b1080, "{b1440} <= {b1080}");

        c.fps = 30;
        let b1440_30 = c.effective_bitrate(2560, 1440);
        assert!(b1440_30 < b1440);

        // HEVC vise plus bas a qualite equivalente.
        c.fps = 60;
        c.codec = VideoCodec::Hevc;
        assert!(c.effective_bitrate(2560, 1440) < b1440);
    }

    #[test]
    fn explicit_bitrate_wins() {
        let c = VideoConfig {
            bitrate: 35_000_000,
            ..VideoConfig::default()
        };
        assert_eq!(c.effective_bitrate(2560, 1440), 35_000_000);
    }

    #[test]
    fn gop_follows_fps_and_interval() {
        let mut c = VideoConfig {
            fps: 60,
            keyframe_interval_secs: 2.0,
            ..VideoConfig::default()
        };
        assert_eq!(c.gop_size(), 120);
        c.fps = 30;
        assert_eq!(c.gop_size(), 60);
    }

    #[test]
    fn output_extension_follows_container() {
        let mut c = Config::default();
        c.output.format = ContainerFormat::Mkv;
        c.output.path = PathBuf::from("/tmp/a.mp4");
        c.normalize_output_extension();
        assert_eq!(c.output.path, PathBuf::from("/tmp/a.mkv"));
    }
}
