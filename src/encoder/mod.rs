//! Encodage video et audio.
//!
//! Les deux encodeurs partagent trois principes :
//!
//! - **Ils ne connaissent pas le muxer.** Ils rendent leurs paquets par un
//!   rappel ([`PacketOut`]) ; c'est le pipeline qui decide ou ils vont. Cela
//!   rend les encodeurs testables sans fichier de sortie.
//! - **Ils sont ouverts avant le demarrage de la capture.** Un encodeur qui
//!   refuse de s'ouvrir doit le faire pendant l'initialisation, pas au milieu
//!   d'un enregistrement.
//! - **Ils imposent leur base de temps.** Le pipeline la lit et s'y conforme,
//!   plutot que l'inverse.

use std::fmt::Write as _;

use ffmpeg_next as ff;
use ff::Packet;

use crate::config::Config;
use crate::error::{RecorderError, Result};

pub mod audio;
pub mod hwdetect;
pub mod video;

/// Resolution d'essai du diagnostic. Assez courante pour qu'aucun encodeur ne
/// la refuse pour une raison de taille, et assez grande pour que l'ouverture
/// d'un contexte materiel soit representative.
const PROBE_SIZE: (u32, u32) = (1920, 1080);

/// Diagnostic du materiel et des encodeurs reellement utilisables.
///
/// Rendu sous forme de texte parce que ses deux appelants — l'option `--probe`
/// et l'interface graphique — n'ont en commun que le besoin de l'afficher.
///
/// La derniere section **ouvre veritablement** un encodeur : un nom annonce
/// par ffmpeg ne prouve rien, seule l'ouverture le prouve. C'est le seul
/// moyen de distinguer un pilote present d'un pilote fonctionnel.
pub fn probe_report(cfg: &Config) -> Result<String> {
    ff::init().map_err(|e| RecorderError::PipelineAborted(format!("ffmpeg : {e}")))?;
    let system = hwdetect::detect();
    let mut out = String::new();

    let _ = writeln!(out, "Materiel");
    let _ = writeln!(out, "  CPU              : {}", system.cpu);
    let _ = writeln!(out, "  Fils d'execution : {}", system.cpu_threads);
    if system.gpus.is_empty() {
        let _ = writeln!(out, "  GPU              : aucun detecte");
    }
    for gpu in &system.gpus {
        let _ = writeln!(out, "  GPU              : {gpu}");
    }
    #[cfg(target_os = "linux")]
    let _ = writeln!(
        out,
        "  Session          : {}",
        crate::capture::linux::session_type()
    );

    let _ = writeln!(
        out,
        "\nEncodeurs candidats pour {} (ordre d'essai)",
        cfg.video.codec
    );
    let candidates = hwdetect::candidates(
        cfg.video.codec,
        cfg.video.hardware,
        system.vendor(),
        &cfg.video.encoder,
    );
    for cand in &candidates {
        let _ = writeln!(
            out,
            "  {:<14} {:<10} {}",
            cand.name,
            if cand.accel.is_hardware() {
                "materiel"
            } else {
                "logiciel"
            },
            if ff::encoder::find_by_name(cand.name).is_some() {
                "present dans ffmpeg"
            } else {
                "ABSENT de cette build"
            }
        );
    }

    let (width, height) = PROBE_SIZE;
    let _ = writeln!(out, "\nOuverture reelle a {width}x{height}");
    // L'encodeur impose par la configuration est volontairement ignore ici :
    // le diagnostic doit montrer ce que la selection automatique retiendrait.
    let mut spec_cfg = cfg.video.clone();
    spec_cfg.encoder = String::new();
    match video::VideoEncoder::open(&video::VideoEncoderSpec {
        width,
        height,
        cfg: spec_cfg,
        global_header: true,
        system: system.clone(),
    }) {
        Ok(enc) => {
            let _ = writeln!(
                out,
                "  retenu : {} ({}, {})",
                enc.name(),
                enc.codec(),
                if enc.is_hardware() {
                    "materiel"
                } else {
                    "logiciel"
                }
            );
        }
        Err(e) => {
            let _ = writeln!(out, "  aucun encodeur utilisable : {e}");
        }
    }
    Ok(out)
}

/// Destination d'un paquet encode.
///
/// Le rappel peut echouer (file fermee, disque plein) ; l'erreur remonte
/// jusqu'au thread d'encodage qui arrete proprement.
pub type PacketOut<'a> = Box<dyn FnMut(Packet) -> Result<()> + 'a>;

/// Vrai si l'erreur signifie « rien a lire pour l'instant » ou « fin de flux ».
///
/// `avcodec_receive_packet` utilise `EAGAIN` pour dire qu'il lui faut plus
/// d'entrees, et `AVERROR_EOF` apres une vidange : aucun des deux n'est une
/// panne.
pub fn is_again(e: &ff::Error) -> bool {
    match e {
        ff::Error::Eof => true,
        ff::Error::Other { errno } => *errno == libc::EAGAIN,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eagain_and_eof_are_not_failures() {
        assert!(is_again(&ff::Error::Eof));
        assert!(is_again(&ff::Error::Other { errno: libc::EAGAIN }));
        assert!(!is_again(&ff::Error::Other { errno: libc::EINVAL }));
        assert!(!is_again(&ff::Error::InvalidData));
    }
}
