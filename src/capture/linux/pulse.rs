//! Capture du son systeme sous Linux, via le moniteur de la sortie par defaut.
//!
//! # Pourquoi PulseAudio et pas PipeWire directement
//!
//! Sur une machine moderne, le serveur est PipeWire, mais il expose une
//! interface PulseAudio (`pipewire-pulse`) que tout le monde implemente. Passer
//! par elle nous donne trois choses gratuitement :
//!
//! - la notion de **moniteur de sortie** (`@DEFAULT_MONITOR@`), c'est-a-dire le
//!   son qui sort des haut-parleurs, pas le microphone ;
//! - le suivi automatique du changement de peripherique par defaut ;
//! - une compatibilite avec les systemes encore sous PulseAudio pur.
//!
//! # Horodatage
//!
//! `pa_simple_read` est bloquant et rend un fragment complet. Au retour, on
//! interroge la latence du flux : le **dernier** echantillon du fragment a ete
//! capture il y a `latence` nanosecondes. On en deduit l'instant du premier
//! echantillon :
//!
//! ```text
//! pts(premier echantillon) = maintenant - latence - duree_du_fragment
//! ```
//!
//! Ce timestamp sert uniquement d'**ancrage** et de mesure de derive : les PTS
//! reellement ecrits dans le fichier sont derives du compteur d'echantillons
//! (voir [`crate::timing::AudioClock`]), car le compteur ne comporte aucun
//! jitter d'ordonnancement.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use libpulse_binding::context::{Context as PaContext, FlagSet as PaContextFlags, State as PaState};
use libpulse_binding::def::BufferAttr;
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::sample::{Format as PaFormat, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;

use crate::capture::{AudioChunk, AudioInfo, AudioSink, AudioSource, StopSignal};
use crate::config::Config;
use crate::error::{RecorderError, Result};
use crate::timing::{monotonic_ns, NS_PER_SEC};

/// Nom special resolu par le serveur vers le moniteur de la sortie courante.
const DEFAULT_MONITOR: &str = "@DEFAULT_MONITOR@";

/// Capture du son systeme.
pub struct PulseAudioSource {
    info: AudioInfo,
    handle: Simple,
    /// Nombre d'echantillons par canal et par fragment.
    fragment_frames: usize,
    /// Tolerance de trou avant insertion de silence.
    max_gap_ns: i64,
}

impl PulseAudioSource {
    pub fn open(cfg: &Config) -> Result<Self> {
        let rate = cfg.audio.sample_rate;
        let channels = cfg.audio.channels;
        let spec = Spec {
            format: PaFormat::F32le,
            rate,
            channels: channels as u8,
        };
        if !spec.is_valid() {
            return Err(RecorderError::AudioCapture(format!(
                "format audio invalide : {rate} Hz, {channels} canaux"
            )));
        }

        let fragment_frames = ((rate as usize * cfg.audio.fragment_ms as usize) / 1000).max(1);
        let frame_bytes = spec.frame_size();
        let fragsize = (fragment_frames * frame_bytes) as u32;
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize,
        };

        // Ordre d'essai : peripherique demande, puis moniteur par defaut, puis
        // le premier moniteur trouve par introspection.
        let mut candidates: Vec<String> = Vec::new();
        if !cfg.audio.device.is_empty() {
            candidates.push(cfg.audio.device.clone());
        }
        candidates.push(DEFAULT_MONITOR.to_owned());

        let mut last_err = String::new();
        for dev in candidates.clone() {
            match Simple::new(
                None,
                "rscap",
                Direction::Record,
                Some(&dev),
                "capture du son systeme",
                &spec,
                None,
                Some(&attr),
            ) {
                Ok(handle) => {
                    tracing::info!(device = %dev, rate, channels, "source audio ouverte");
                    return Ok(Self {
                        info: AudioInfo {
                            device: dev,
                            sample_rate: rate,
                            channels,
                        },
                        handle,
                        fragment_frames,
                        max_gap_ns: cfg.audio.max_drift_ms as i64 * 1_000_000,
                    });
                }
                Err(e) => last_err = format!("{dev} : {e}"),
            }
        }

        // Dernier recours : demander au serveur la liste des moniteurs.
        if let Ok(monitors) = list_monitor_sources() {
            for dev in monitors {
                if candidates.iter().any(|c| c == &dev) {
                    continue;
                }
                if let Ok(handle) = Simple::new(
                    None,
                    "rscap",
                    Direction::Record,
                    Some(&dev),
                    "capture du son systeme",
                    &spec,
                    None,
                    Some(&attr),
                ) {
                    tracing::info!(device = %dev, "source audio ouverte (repli par introspection)");
                    return Ok(Self {
                        info: AudioInfo {
                            device: dev,
                            sample_rate: rate,
                            channels,
                        },
                        handle,
                        fragment_frames,
                        max_gap_ns: cfg.audio.max_drift_ms as i64 * 1_000_000,
                    });
                }
            }
        }

        Err(RecorderError::AudioCapture(format!(
            "aucun moniteur de sortie disponible ({last_err}). \
             Listez les sources avec `rscap --list-audio` ou `pactl list short sources`, \
             puis renseignez `audio.device` dans la configuration"
        )))
    }
}

impl AudioSource for PulseAudioSource {
    fn info(&self) -> AudioInfo {
        self.info.clone()
    }

    fn run(&mut self, sink: AudioSink, stop: Arc<StopSignal>) -> Result<()> {
        let channels = self.info.channels as usize;
        let rate = self.info.sample_rate as i64;
        let frames = self.fragment_frames;
        let fragment_ns = frames as i64 * NS_PER_SEC / rate;

        let mut raw = vec![0u8; frames * channels * size_of::<f32>()];
        // Fin theorique du fragment precedent : sert a detecter les trous.
        let mut expected_next_ns: Option<i64> = None;
        let mut consecutive_errors = 0u32;

        while !stop.is_stopped() {
            if let Err(e) = self.handle.read(&mut raw) {
                consecutive_errors += 1;
                tracing::warn!(error = %e, consecutive_errors, "lecture audio en echec");
                if consecutive_errors >= 5 {
                    return Err(RecorderError::AudioDeviceLost(format!(
                        "{} lectures consecutives en echec : {e}",
                        consecutive_errors
                    )));
                }
                continue;
            }
            consecutive_errors = 0;

            let now = monotonic_ns();
            // Latence du flux : age du dernier echantillon du fragment.
            let latency_ns = self
                .handle
                .get_latency()
                .map(|l| l.0 as i64 * 1_000)
                .unwrap_or(0);
            let pts_ns = now - latency_ns - fragment_ns;

            let discontinuity = match expected_next_ns {
                Some(expected) => (pts_ns - expected).abs() > self.max_gap_ns,
                None => false,
            };
            if discontinuity {
                tracing::warn!(
                    gap_ms = (pts_ns - expected_next_ns.unwrap_or(pts_ns)) / 1_000_000,
                    "discontinuite audio detectee"
                );
            }
            expected_next_ns = Some(pts_ns + fragment_ns);

            // Reinterpretation des octets en `f32` natifs : le serveur nous a
            // deja livre du F32 petit-boutiste, identique a la representation
            // machine sur les cibles visees.
            let mut samples = vec![0.0f32; frames * channels];
            for (out, chunk) in samples.iter_mut().zip(raw.chunks_exact(4)) {
                let bytes = [chunk[0], chunk[1], chunk[2], chunk[3]];
                *out = f32::from_le_bytes(bytes);
            }

            sink.stats()
                .record_audio_latency_us((latency_ns / 1000).max(0) as u64);
            sink.stats().audio_samples(frames as u64);

            if !sink.submit(AudioChunk::new(
                samples,
                self.info.channels,
                pts_ns,
                now,
                discontinuity,
            )) {
                break;
            }
        }
        Ok(())
    }
}

/// Liste les sources de type *moniteur* connues du serveur.
///
/// Utilise pour le diagnostic (`--list-audio`) et comme ultime repli si
/// `@DEFAULT_MONITOR@` n'est pas resolu.
pub fn list_monitor_sources() -> Result<Vec<String>> {
    let err = |m: String| RecorderError::AudioCapture(m);

    let mut mainloop =
        Mainloop::new().ok_or_else(|| err("boucle PulseAudio indisponible".into()))?;
    let mut context = PaContext::new(&mainloop, "rscap-probe")
        .ok_or_else(|| err("contexte PulseAudio indisponible".into()))?;
    context
        .connect(None, PaContextFlags::NOFLAGS, None)
        .map_err(|e| err(format!("connexion au serveur audio : {e}")))?;

    // Attente de l'etablissement de la connexion.
    loop {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) => return Err(err("boucle audio interrompue".into())),
            IterateResult::Err(e) => return Err(err(format!("boucle audio : {e}"))),
        }
        match context.get_state() {
            PaState::Ready => break,
            PaState::Failed | PaState::Terminated => {
                return Err(err("connexion au serveur audio refusee".into()))
            }
            _ => {}
        }
    }

    let found: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let done = Rc::new(RefCell::new(false));
    {
        let found_cb = Rc::clone(&found);
        let done_cb = Rc::clone(&done);
        let op = context
            .introspect()
            .get_source_info_list(move |result| match result {
                ListResult::Item(info) => {
                    // `monitor_of_sink` renseigne = c'est bien la sortie d'un
                    // peripherique de lecture, pas une entree microphone.
                    if info.monitor_of_sink.is_some() {
                        if let Some(name) = info.name.as_ref() {
                            found_cb.borrow_mut().push(name.to_string());
                        }
                    }
                }
                ListResult::End | ListResult::Error => *done_cb.borrow_mut() = true,
            });
        // L'operation est pilotee par la boucle : on itere jusqu'a la fin.
        while !*done.borrow() {
            match mainloop.iterate(true) {
                IterateResult::Success(_) => {}
                IterateResult::Quit(_) | IterateResult::Err(_) => break,
            }
        }
        drop(op);
    }

    context.disconnect();
    let list = found.borrow().clone();
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_size_matches_the_requested_duration() {
        // 10 ms a 48 kHz = 480 echantillons par canal.
        let mut cfg = Config::default();
        cfg.audio.fragment_ms = 10;
        cfg.audio.sample_rate = 48_000;
        let frames = ((cfg.audio.sample_rate as usize * cfg.audio.fragment_ms as usize) / 1000).max(1);
        assert_eq!(frames, 480);
    }

    #[test]
    fn monitor_enumeration_does_not_panic_without_a_server() {
        // Sur une machine sans serveur audio, la fonction doit rendre une
        // erreur propre, jamais paniquer.
        let _ = list_monitor_sources();
    }
}
