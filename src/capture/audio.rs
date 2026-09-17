//! Selection de la source audio systeme selon la plateforme.

use crate::config::Config;
use crate::error::Result;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
use crate::error::RecorderError;

use super::AudioSource;

/// Ouvre la capture du son systeme (sortie, pas microphone).
#[allow(unused_variables)]
pub fn open(cfg: &Config) -> Result<Box<dyn AudioSource>> {
    #[cfg(target_os = "linux")]
    {
        super::linux::open_audio(cfg)
    }
    #[cfg(target_os = "windows")]
    {
        super::windows::open_audio(cfg)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(RecorderError::Unsupported(format!(
            "aucune capture audio implementee pour {}",
            std::env::consts::OS
        )))
    }
}
