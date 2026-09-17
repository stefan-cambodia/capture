//! Backend Linux : portail xdg + PipeWire pour la video, moniteur PulseAudio
//! pour le son systeme.
//!
//! Ce choix couvre Wayland **et** X11 : sous Wayland c'est la seule voie
//! possible (le protocole interdit a une application de lire l'ecran
//! directement), et sous X11 le portail delegue a la meme infrastructure
//! PipeWire. Une implementation X11/XShm separee n'apporterait donc rien,
//! sinon un second chemin a maintenir — et elle ne fonctionne pas du tout
//! sous Wayland, y compris via XWayland, qui ne voit que les fenetres X11.

use crate::config::Config;
use crate::error::Result;

use super::{AudioSource, ScreenSource};

pub mod portal;
pub mod pulse;
pub mod pw_screen;

pub fn open_screen(cfg: &Config) -> Result<Box<dyn ScreenSource>> {
    Ok(Box::new(pw_screen::PipeWireScreenSource::open(cfg)?))
}

pub fn open_audio(cfg: &Config) -> Result<Box<dyn AudioSource>> {
    Ok(Box::new(pulse::PulseAudioSource::open(cfg)?))
}

/// Indique si la session courante est Wayland, pour le diagnostic.
pub fn session_type() -> &'static str {
    match std::env::var("XDG_SESSION_TYPE").as_deref() {
        Ok("wayland") => "wayland",
        Ok("x11") => "x11",
        _ => "inconnu",
    }
}
