//! Selection de la source d'ecran selon la plateforme.
//!
//! Aucune logique specifique a un systeme ne figure ici : ce module ne fait
//! que router vers le sous-module correspondant.

use crate::config::Config;
use crate::error::Result;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
use crate::error::RecorderError;

use super::ScreenSource;

/// Ouvre la capture de l'ecran principal.
#[allow(unused_variables)]
pub fn open(cfg: &Config) -> Result<Box<dyn ScreenSource>> {
    #[cfg(target_os = "linux")]
    {
        super::linux::open_screen(cfg)
    }
    #[cfg(target_os = "windows")]
    {
        super::windows::open_screen(cfg)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(RecorderError::Unsupported(format!(
            "aucune capture d'ecran implementee pour {}",
            std::env::consts::OS
        )))
    }
}
