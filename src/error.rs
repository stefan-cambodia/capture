//! Types d'erreur du projet.
//!
//! Aucun chemin critique n'utilise `unwrap()`/`expect()` : tout remonte par
//! `Result<T, RecorderError>`. Les erreurs systeme les plus significatives
//! pour un enregistreur (disque plein, peripherique perdu) ont leur propre
//! variante afin que la couche superieure puisse reagir sans inspecter un
//! message texte.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Erreur unique du recorder.
#[derive(Debug, Error)]
pub enum RecorderError {
    #[error("configuration invalide : {0}")]
    Config(String),

    #[error("capture ecran indisponible : {0}")]
    ScreenCapture(String),

    #[error("capture audio indisponible : {0}")]
    AudioCapture(String),

    #[error("le peripherique audio a disparu : {0}")]
    AudioDeviceLost(String),

    #[error("aucun encodeur video utilisable : {0}")]
    NoUsableEncoder(String),

    #[error("erreur d'encodage ({stage}) : {source}")]
    Encode {
        stage: &'static str,
        #[source]
        source: ffmpeg_next::Error,
    },

    #[error("erreur de muxage : {0}")]
    Mux(String),

    #[error("disque plein lors de l'ecriture de {path}")]
    DiskFull { path: PathBuf },

    #[error("erreur d'ecriture disque sur {path} : {source}")]
    DiskWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("resolution non supportee : {width}x{height} ({reason})")]
    UnsupportedResolution {
        width: u32,
        height: u32,
        reason: &'static str,
    },

    #[error("fonctionnalite non supportee sur cette plateforme : {0}")]
    Unsupported(String),

    #[error("le pipeline s'est arrete prematurement : {0}")]
    PipelineAborted(String),

    #[error("un thread du pipeline a panique : {0}")]
    ThreadPanic(String),

    #[error(transparent)]
    Ffmpeg(#[from] ffmpeg_next::Error),

    #[error(transparent)]
    Io(#[from] io::Error),
}

impl RecorderError {
    /// Classe une erreur d'E/S en `DiskFull` ou `DiskWrite` selon `errno`.
    pub fn from_write(path: impl Into<PathBuf>, source: io::Error) -> Self {
        let path = path.into();
        // ENOSPC est la seule condition que l'on veut distinguer : elle est
        // previsible sur une longue session et merite un message clair.
        if source.raw_os_error() == Some(libc::ENOSPC) {
            RecorderError::DiskFull { path }
        } else {
            RecorderError::DiskWrite { path, source }
        }
    }

    /// Vrai si poursuivre l'enregistrement n'a aucun sens.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            RecorderError::DiskFull { .. }
                | RecorderError::DiskWrite { .. }
                | RecorderError::Mux(_)
                | RecorderError::NoUsableEncoder(_)
                | RecorderError::PipelineAborted(_)
        )
    }
}

/// Alias de confort.
pub type Result<T> = std::result::Result<T, RecorderError>;
