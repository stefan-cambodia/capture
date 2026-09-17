//! Backend Windows — **non implemente**.
//!
//! L'architecture prevue, derriere les memes traits que le backend Linux :
//!
//! | Etage | API |
//! |---|---|
//! | Capture ecran | `Windows.Graphics.Capture` (WinRT), repli `IDXGIOutputDuplication` |
//! | Surface | `ID3D11Texture2D` (B8G8R8A8) |
//! | Conversion | `ID3D11VideoProcessor` : BGRA -> NV12 sur le GPU |
//! | Encodage | `h264_nvenc` / `hevc_nvenc`, `h264_amf`, `h264_qsv`, repli `libx264` |
//! | Audio | WASAPI en mode *loopback* sur le peripherique de rendu par defaut |
//! | Horloge | `QueryPerformanceCounter`, et `QPCTime` du `Direct3D11CaptureFrame` |
//!
//! Rien n'est fourni ici plutot qu'un squelette non compile : ce module serait
//! le seul code du projet qui n'a jamais ete compile ni execute, et un
//! squelette qui ne compile pas coute plus de temps qu'il n'en fait gagner.
//! Les traits [`crate::capture::ScreenSource`] et
//! [`crate::capture::AudioSource`] sont le seul point d'ancrage necessaire :
//! tout le reste du pipeline (cadencement, files, encodeurs, muxer,
//! statistiques) est deja independant de la plateforme.

use crate::config::Config;
use crate::error::{RecorderError, Result};

use super::{AudioSource, ScreenSource};

pub fn open_screen(_cfg: &Config) -> Result<Box<dyn ScreenSource>> {
    Err(RecorderError::Unsupported(
        "backend Windows (Windows Graphics Capture) non implemente".into(),
    ))
}

pub fn open_audio(_cfg: &Config) -> Result<Box<dyn AudioSource>> {
    Err(RecorderError::Unsupported(
        "backend Windows (WASAPI loopback) non implemente".into(),
    ))
}
