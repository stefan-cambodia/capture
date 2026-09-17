//! Detection du materiel et choix de l'encodeur.
//!
//! La regle est simple : **on ne fait pas confiance a la presence d'un nom
//! d'encodeur dans ffmpeg**. `ffmpeg -encoders` liste `h264_nvenc` sur une
//! machine sans carte NVIDIA, et `h264_vaapi` sans noeud de rendu accessible.
//! Le seul test fiable est d'ouvrir l'encodeur avec les parametres reels ;
//! c'est ce que fait [`crate::encoder::video::VideoEncoder::open`], en
//! parcourant la liste de candidats produite ici.
//!
//! La detection sert donc a **ordonner** les candidats, pas a les filtrer :
//! commencer par l'encodeur du GPU present evite des ouvertures inutiles.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::config::{HardwarePolicy, VideoCodec};

/// Fabricant du GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Intel,
    Amd,
    Nvidia,
    Other,
}

impl GpuVendor {
    fn from_pci_id(id: u32) -> Self {
        match id {
            0x8086 => GpuVendor::Intel,
            0x1002 | 0x1022 => GpuVendor::Amd,
            0x10de => GpuVendor::Nvidia,
            _ => GpuVendor::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            GpuVendor::Intel => "Intel",
            GpuVendor::Amd => "AMD",
            GpuVendor::Nvidia => "NVIDIA",
            GpuVendor::Other => "inconnu",
        }
    }
}

/// Un GPU vu par le systeme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuInfo {
    pub vendor: GpuVendor,
    pub name: String,
    /// Noeud de rendu DRM (`/dev/dri/renderD128`), quand il existe.
    pub render_node: Option<PathBuf>,
}

impl fmt::Display for GpuInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.vendor.as_str())?;
        if let Some(node) = &self.render_node {
            write!(f, " via {}", node.display())?;
        }
        Ok(())
    }
}

/// Inventaire materiel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemInfo {
    pub cpu: String,
    pub cpu_threads: usize,
    pub gpus: Vec<GpuInfo>,
}

impl SystemInfo {
    /// GPU retenu pour l'encodage : le premier disposant d'un noeud de rendu.
    pub fn primary_gpu(&self) -> Option<&GpuInfo> {
        self.gpus
            .iter()
            .find(|g| g.render_node.is_some())
            .or_else(|| self.gpus.first())
    }

    pub fn vendor(&self) -> GpuVendor {
        self.primary_gpu().map_or(GpuVendor::Other, |g| g.vendor)
    }

    /// Noeud de rendu a utiliser pour VAAPI/QSV.
    pub fn render_node(&self) -> Option<&Path> {
        self.primary_gpu()
            .and_then(|g| g.render_node.as_deref())
    }
}

/// Inspecte CPU et GPU.
pub fn detect() -> SystemInfo {
    SystemInfo {
        cpu: cpu_model(),
        cpu_threads: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        gpus: detect_gpus(),
    }
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_owned())
        })
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned())
}

#[cfg(target_os = "linux")]
fn detect_gpus() -> Vec<GpuInfo> {
    let mut gpus = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return gpus;
    };
    let mut cards: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("card") && !n.contains('-'))
        })
        .collect();
    cards.sort();

    for card in cards {
        let device = card.join("device");
        let vendor_id = std::fs::read_to_string(device.join("vendor"))
            .ok()
            .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok());
        let Some(vendor_id) = vendor_id else { continue };

        // Le nom lisible n'est pas expose par sysfs ; on compose un
        // identifiant stable a partir du couple vendeur/peripherique.
        let device_id = std::fs::read_to_string(device.join("device"))
            .ok()
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        let vendor = GpuVendor::from_pci_id(vendor_id);
        let name = format!("{} {}", vendor.as_str(), device_id);

        gpus.push(GpuInfo {
            vendor,
            name,
            render_node: find_render_node(&device),
        });
    }
    gpus
}

#[cfg(not(target_os = "linux"))]
fn detect_gpus() -> Vec<GpuInfo> {
    Vec::new()
}

/// Cherche le `renderD*` associe a un peripherique PCI donne.
fn find_render_node(device_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(device_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("renderD") {
            let node = PathBuf::from("/dev/dri").join(name.as_ref());
            if node.exists() {
                return Some(node);
            }
        }
        // Certains pilotes imbriquent les noeuds sous `drm/`.
        if name == "drm" {
            if let Ok(sub) = std::fs::read_dir(entry.path()) {
                for e in sub.flatten() {
                    let n = e.file_name();
                    let n = n.to_string_lossy();
                    if n.starts_with("renderD") {
                        let node = PathBuf::from("/dev/dri").join(n.as_ref());
                        if node.exists() {
                            return Some(node);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Type d'acceleration d'un encodeur, qui determine la facon de lui fournir
/// les images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceleration {
    /// Encodeur logiciel : images en memoire centrale.
    Software,
    /// L'encodeur prend des images logicielles et les televerse lui-meme
    /// (NVENC, AMF). Rien de special a faire.
    HardwareDirect,
    /// L'encodeur exige des surfaces GPU : il faut un `AVHWFramesContext`
    /// (VAAPI, QSV).
    HardwareFrames,
}

impl Acceleration {
    pub fn is_hardware(self) -> bool {
        !matches!(self, Acceleration::Software)
    }
}

/// Un candidat a essayer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub name: &'static str,
    pub codec: VideoCodec,
    pub accel: Acceleration,
}

const fn c(name: &'static str, codec: VideoCodec, accel: Acceleration) -> Candidate {
    Candidate { name, codec, accel }
}

/// Encodeurs materiels d'un codec, tries pour un fabricant donne.
fn hardware_for(codec: VideoCodec, vendor: GpuVendor) -> Vec<Candidate> {
    use Acceleration::{HardwareDirect, HardwareFrames};
    let (vaapi, qsv, nvenc, amf) = match codec {
        VideoCodec::H264 => ("h264_vaapi", "h264_qsv", "h264_nvenc", "h264_amf"),
        VideoCodec::Hevc => ("hevc_vaapi", "hevc_qsv", "hevc_nvenc", "hevc_amf"),
        VideoCodec::Av1 => ("av1_vaapi", "av1_qsv", "av1_nvenc", "av1_amf"),
    };
    let vaapi = c(vaapi, codec, HardwareFrames);
    let qsv = c(qsv, codec, HardwareFrames);
    let nvenc = c(nvenc, codec, HardwareDirect);
    let amf = c(amf, codec, HardwareDirect);

    match vendor {
        // VAAPI d'abord sur Intel : le chemin QSV ajoute une couche (libvpl)
        // pour un gain nul sur une capture d'ecran.
        GpuVendor::Intel => vec![vaapi, qsv],
        GpuVendor::Amd => vec![vaapi, amf],
        GpuVendor::Nvidia => vec![nvenc, vaapi],
        GpuVendor::Other => vec![vaapi, qsv, nvenc, amf],
    }
}

/// Encodeurs logiciels d'un codec, du plus rapide au plus lent.
fn software_for(codec: VideoCodec) -> Vec<Candidate> {
    use Acceleration::Software;
    match codec {
        VideoCodec::H264 => vec![c("libx264", VideoCodec::H264, Software)],
        VideoCodec::Hevc => vec![c("libx265", VideoCodec::Hevc, Software)],
        VideoCodec::Av1 => vec![
            c("libsvtav1", VideoCodec::Av1, Software),
            c("librav1e", VideoCodec::Av1, Software),
            c("libaom-av1", VideoCodec::Av1, Software),
        ],
    }
}

/// Construit la liste ordonnee des encodeurs a essayer.
///
/// L'ordre suit la priorite demandee au projet :
///
/// ```text
/// codec demande, materiel
///   -> H.264 materiel
///   -> HEVC materiel
///   -> AV1 materiel
///   -> codec demande, logiciel
///   -> H.264 logiciel
/// ```
///
/// `HardwarePolicy::Force` coupe la liste avant les candidats logiciels ;
/// `HardwarePolicy::Off` ne garde que ceux-ci.
pub fn candidates(
    requested: VideoCodec,
    policy: HardwarePolicy,
    vendor: GpuVendor,
    forced_name: &str,
) -> Vec<Candidate> {
    // Un encodeur impose court-circuite toute la logique.
    if !forced_name.is_empty() {
        let accel = accel_for_name(forced_name);
        let codec = codec_for_name(forced_name).unwrap_or(requested);
        return vec![Candidate {
            // `Candidate.name` est `&'static str` : on retrouve le nom
            // statique dans la table, sinon on echoue proprement plus haut.
            name: static_name(forced_name).unwrap_or("?"),
            codec,
            accel,
        }];
    }

    let mut out: Vec<Candidate> = Vec::new();
    let push = |list: Vec<Candidate>, out: &mut Vec<Candidate>| {
        for cand in list {
            if !out.iter().any(|c| c.name == cand.name) {
                out.push(cand);
            }
        }
    };

    if policy != HardwarePolicy::Off {
        push(hardware_for(requested, vendor), &mut out);
        for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
            push(hardware_for(codec, vendor), &mut out);
        }
    }
    if policy != HardwarePolicy::Force {
        push(software_for(requested), &mut out);
        push(software_for(VideoCodec::H264), &mut out);
    }
    out
}

/// Tous les noms d'encodeurs connus, pour retrouver un `&'static str`.
const ALL_NAMES: &[(&str, VideoCodec, Acceleration)] = &[
    ("h264_vaapi", VideoCodec::H264, Acceleration::HardwareFrames),
    ("hevc_vaapi", VideoCodec::Hevc, Acceleration::HardwareFrames),
    ("av1_vaapi", VideoCodec::Av1, Acceleration::HardwareFrames),
    ("h264_qsv", VideoCodec::H264, Acceleration::HardwareFrames),
    ("hevc_qsv", VideoCodec::Hevc, Acceleration::HardwareFrames),
    ("av1_qsv", VideoCodec::Av1, Acceleration::HardwareFrames),
    ("h264_nvenc", VideoCodec::H264, Acceleration::HardwareDirect),
    ("hevc_nvenc", VideoCodec::Hevc, Acceleration::HardwareDirect),
    ("av1_nvenc", VideoCodec::Av1, Acceleration::HardwareDirect),
    ("h264_amf", VideoCodec::H264, Acceleration::HardwareDirect),
    ("hevc_amf", VideoCodec::Hevc, Acceleration::HardwareDirect),
    ("av1_amf", VideoCodec::Av1, Acceleration::HardwareDirect),
    ("libx264", VideoCodec::H264, Acceleration::Software),
    ("libx265", VideoCodec::Hevc, Acceleration::Software),
    ("libsvtav1", VideoCodec::Av1, Acceleration::Software),
    ("librav1e", VideoCodec::Av1, Acceleration::Software),
    ("libaom-av1", VideoCodec::Av1, Acceleration::Software),
];

fn static_name(name: &str) -> Option<&'static str> {
    ALL_NAMES.iter().find(|(n, _, _)| *n == name).map(|(n, _, _)| *n)
}

fn codec_for_name(name: &str) -> Option<VideoCodec> {
    ALL_NAMES.iter().find(|(n, _, _)| *n == name).map(|(_, c, _)| *c)
}

/// Deduit le mode d'alimentation d'un encodeur d'apres son suffixe.
pub fn accel_for_name(name: &str) -> Acceleration {
    if let Some((_, _, a)) = ALL_NAMES.iter().find(|(n, _, _)| *n == name) {
        return *a;
    }
    if name.ends_with("_vaapi") || name.ends_with("_qsv") {
        Acceleration::HardwareFrames
    } else if name.ends_with("_nvenc") || name.ends_with("_amf") || name.ends_with("_mf") {
        Acceleration::HardwareDirect
    } else {
        Acceleration::Software
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_reports_a_cpu_and_thread_count() {
        let info = detect();
        assert!(!info.cpu.is_empty());
        assert!(info.cpu_threads >= 1);
    }

    #[test]
    fn intel_prefers_vaapi_then_qsv() {
        let list = candidates(VideoCodec::H264, HardwarePolicy::Auto, GpuVendor::Intel, "");
        assert_eq!(list[0].name, "h264_vaapi");
        assert_eq!(list[1].name, "h264_qsv");
    }

    #[test]
    fn nvidia_prefers_nvenc() {
        let list = candidates(VideoCodec::H264, HardwarePolicy::Auto, GpuVendor::Nvidia, "");
        assert_eq!(list[0].name, "h264_nvenc");
    }

    #[test]
    fn fallback_order_is_hw_h264_hevc_av1_then_software() {
        let list = candidates(VideoCodec::H264, HardwarePolicy::Auto, GpuVendor::Intel, "");
        let names: Vec<&str> = list.iter().map(|c| c.name).collect();
        let pos = |n: &str| names.iter().position(|x| *x == n);
        assert!(pos("h264_vaapi") < pos("hevc_vaapi"), "{names:?}");
        assert!(pos("hevc_vaapi") < pos("av1_vaapi"), "{names:?}");
        assert!(pos("av1_vaapi") < pos("libx264"), "{names:?}");
        // Le dernier recours est toujours un encodeur logiciel H.264.
        assert!(names.contains(&"libx264"));
    }

    #[test]
    fn requested_codec_comes_first_even_if_not_h264() {
        let list = candidates(VideoCodec::Hevc, HardwarePolicy::Auto, GpuVendor::Intel, "");
        assert_eq!(list[0].name, "hevc_vaapi");
        // H.264 materiel reste propose juste apres, comme repli.
        assert!(list.iter().any(|c| c.name == "h264_vaapi"));
    }

    #[test]
    fn force_policy_excludes_software() {
        let list = candidates(VideoCodec::H264, HardwarePolicy::Force, GpuVendor::Intel, "");
        assert!(!list.is_empty());
        assert!(list.iter().all(|c| c.accel.is_hardware()), "{list:?}");
    }

    #[test]
    fn off_policy_excludes_hardware() {
        let list = candidates(VideoCodec::H264, HardwarePolicy::Off, GpuVendor::Intel, "");
        assert!(!list.is_empty());
        assert!(list.iter().all(|c| !c.accel.is_hardware()), "{list:?}");
    }

    #[test]
    fn an_explicit_encoder_short_circuits_the_list() {
        let list = candidates(VideoCodec::H264, HardwarePolicy::Auto, GpuVendor::Intel, "libx264");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "libx264");
        assert_eq!(list[0].accel, Acceleration::Software);
    }

    #[test]
    fn no_duplicates_in_the_candidate_list() {
        for vendor in [GpuVendor::Intel, GpuVendor::Amd, GpuVendor::Nvidia, GpuVendor::Other] {
            let list = candidates(VideoCodec::H264, HardwarePolicy::Auto, vendor, "");
            let mut names: Vec<&str> = list.iter().map(|c| c.name).collect();
            let before = names.len();
            names.sort_unstable();
            names.dedup();
            assert_eq!(before, names.len(), "doublons pour {vendor:?}");
        }
    }

    #[test]
    fn acceleration_is_deduced_from_the_suffix() {
        assert_eq!(accel_for_name("h264_vaapi"), Acceleration::HardwareFrames);
        assert_eq!(accel_for_name("hevc_nvenc"), Acceleration::HardwareDirect);
        assert_eq!(accel_for_name("libx264"), Acceleration::Software);
        // Un nom inconnu mais suffixe reste correctement classe.
        assert_eq!(accel_for_name("vp9_vaapi"), Acceleration::HardwareFrames);
    }
}
