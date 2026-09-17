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

use ffmpeg_next as ff;
use ff::Packet;

use crate::error::Result;

pub mod audio;
pub mod hwdetect;
pub mod video;

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
