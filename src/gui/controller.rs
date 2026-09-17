//! Thread de commande : il possede l'enregistrement, l'interface ne le touche
//! jamais.
//!
//! # Pourquoi un thread
//!
//! Trois operations de cette application bloquent longtemps : la negociation
//! du portail xdg (l'utilisateur doit choisir un ecran), l'ouverture d'un
//! encodeur materiel, et la finalisation du fichier. Executees sur le thread
//! principal, elles figeraient la fenetre — GTK ne redessine rien tant que sa
//! boucle d'evenements n'a pas la main.
//!
//! L'interface envoie donc des [`Command`] et recoit des [`Event`]. Elle ne
//! partage avec le pipeline aucune donnee mutable : les statistiques voyagent
//! par copie ([`StatsSnapshot`] est `Copy`), ce qui evite tout verrou entre le
//! rendu et le chemin chaud.
//!
//! # Garantie d'arret
//!
//! Le thread ne detruit jamais un [`Recording`] sans appeler `stop()` : c'est
//! `stop()` qui vide les files, termine l'encodage et ecrit la fin du
//! conteneur. Un fichier laisse sans finalisation serait illisible.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};

use crate::bench::{self, BenchOptions};
use crate::capture::{AudioSource, ScreenSource};
use crate::config::Config;
use crate::error::Result;
use crate::performance::StatsSnapshot;
use crate::pipeline::{Recording, RecordingInfo};
use crate::timing::monotonic_ns;

/// Periode d'emission des statistiques. Cinq hertz : au-dela, l'oeil ne suit
/// plus et chaque photo coute un parcours d'histogrammes.
const TICK: Duration = Duration::from_millis(200);

/// Resolution du generateur d'images, quand la source synthetique remplace
/// l'ecran reel.
const SYNTHETIC_SIZE: (u32, u32) = (2560, 1440);

/// Ce que l'interface demande.
pub enum Command {
    Start(Box<StartRequest>),
    /// Arret demande par l'utilisateur. Ignoree hors enregistrement.
    Stop,
    Probe(Box<Config>),
    ListAudioDevices,
    ForgetPermission,
    Benchmark(Box<Config>, BenchOptions),
    /// Fin du thread. Un enregistrement en cours est finalise avant.
    Shutdown,
}

/// Tout ce qu'il faut pour demarrer, fige au moment du clic.
pub struct StartRequest {
    pub cfg: Config,
    /// Generateur d'images au lieu de l'ecran reel (pas de portail).
    pub synthetic_video: bool,
    /// Generateur audio au lieu du moniteur systeme.
    pub synthetic_audio: bool,
    /// Arret automatique, en secondes.
    pub duration: Option<f64>,
}

/// Ce que le thread rapporte.
pub enum Event {
    /// Ouverture des sources en cours : le portail peut demander un ecran.
    Opening,
    Started(Box<RecordingInfo>),
    Tick(StatsSnapshot),
    /// Le fichier est ecrit, ferme et synchronise sur le disque.
    Finished {
        path: PathBuf,
        stats: StatsSnapshot,
        info: Box<RecordingInfo>,
    },
    /// Echec d'une operation. L'interface revient a l'etat de repos.
    Failed(String),
    /// Information sans consequence sur l'etat (audio indisponible, …).
    Notice(String),
    AudioDevices(Vec<String>),
    Report(String),
    /// Rapport de banc d'essai, avec le verdict deja calcule.
    BenchReport { text: String, sustained: bool },
    /// Le thread est occupe par une operation longue et non interruptible.
    Busy(bool),
}

/// Poignee de l'interface sur le thread.
pub struct Controller {
    commands: Sender<Command>,
    events: Receiver<Event>,
    handle: Option<JoinHandle<()>>,
}

impl Controller {
    pub fn spawn() -> Self {
        // Files non bornees : les messages sont rares et minuscules, et une
        // file bornee pourrait refuser un ordre d'arret.
        let (cmd_tx, cmd_rx) = unbounded();
        let (ev_tx, ev_rx) = unbounded();
        let handle = thread::Builder::new()
            .name("rscap-gui-ctl".into())
            .spawn(move || run(&cmd_rx, &ev_tx))
            .expect("thread de commande");
        Self {
            commands: cmd_tx,
            events: ev_rx,
            handle: Some(handle),
        }
    }

    /// Envoie un ordre. Un thread mort n'est pas une erreur fatale pour
    /// l'interface : elle continue d'afficher son dernier etat.
    pub fn send(&self, cmd: Command) {
        let _ = self.commands.send(cmd);
    }

    /// Evenements en attente, sans jamais bloquer la boucle GTK.
    pub fn drain(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(h) = self.handle.take() {
            // On attend : c'est ici que se termine la finalisation d'un
            // fichier si la fenetre est fermee pendant un enregistrement.
            let _ = h.join();
        }
    }
}

fn run(cmd: &Receiver<Command>, ev: &Sender<Event>) {
    while let Ok(command) = cmd.recv() {
        match command {
            Command::Start(req) => {
                if let Err(e) = record(*req, cmd, ev) {
                    let _ = ev.send(Event::Failed(chain(&e)));
                }
            }
            Command::Probe(cfg) => {
                let _ = ev.send(Event::Busy(true));
                match crate::encoder::probe_report(&cfg) {
                    Ok(text) => {
                        let _ = ev.send(Event::Report(text));
                    }
                    Err(e) => {
                        let _ = ev.send(Event::Failed(chain(&e)));
                    }
                }
                let _ = ev.send(Event::Busy(false));
            }
            Command::ListAudioDevices => match list_audio_devices() {
                Ok(list) => {
                    let _ = ev.send(Event::AudioDevices(list));
                }
                Err(e) => {
                    let _ = ev.send(Event::Notice(format!("peripheriques audio : {}", chain(&e))));
                }
            },
            Command::ForgetPermission => match forget_permission() {
                Ok(()) => {
                    let _ = ev.send(Event::Notice(
                        "Autorisation oubliee : l'ecran sera redemande.".into(),
                    ));
                }
                Err(e) => {
                    let _ = ev.send(Event::Failed(chain(&e)));
                }
            },
            Command::Benchmark(cfg, opts) => {
                let _ = ev.send(Event::Busy(true));
                match bench::run(&cfg, &opts) {
                    Ok(report) => {
                        let _ = ev.send(Event::BenchReport {
                            text: report.to_text(),
                            sustained: report.target_sustained(),
                        });
                    }
                    Err(e) => {
                        let _ = ev.send(Event::Failed(chain(&e)));
                    }
                }
                let _ = ev.send(Event::Busy(false));
            }
            Command::Stop => { /* aucun enregistrement en cours */ }
            Command::Shutdown => return,
        }
    }
}

/// Un enregistrement complet, du portail au fichier finalise.
fn record(req: StartRequest, cmd: &Receiver<Command>, ev: &Sender<Event>) -> Result<()> {
    let _ = ev.send(Event::Opening);

    let mut cfg = req.cfg;
    // Meme ordre que la ligne de commande : l'extension suit le conteneur,
    // puis tout est valide avant qu'une ressource ne soit ouverte.
    cfg.normalize_output_extension();
    cfg.validate()?;

    let screen = open_screen(&cfg, req.synthetic_video)?;
    let audio = open_audio(&cfg, req.synthetic_audio, ev);

    let recording = Recording::start(&cfg, screen, audio)?;
    let info = recording.info().clone();
    // Les compteurs survivent a `stop()`, qui consomme l'enregistrement : le
    // resume final doit inclure la finalisation du conteneur.
    let stats = Arc::clone(recording.stats());
    let _ = ev.send(Event::Started(Box::new(info.clone())));

    let deadline = req.duration.map(|d| monotonic_ns() + (d * 1e9) as i64);
    let mut shutdown = false;
    loop {
        // `recv_timeout` sert des deux cotes : il cadence l'envoi des
        // statistiques *et* rend l'arret immediat, sans sondage.
        match cmd.recv_timeout(TICK) {
            Ok(Command::Stop) => break,
            Ok(Command::Shutdown) => {
                shutdown = true;
                break;
            }
            // Le reste n'a pas de sens pendant un enregistrement : le materiel
            // est occupe et la configuration est figee.
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                shutdown = true;
                break;
            }
        }
        let _ = ev.send(Event::Tick(stats.snapshot()));
        if deadline.is_some_and(|d| monotonic_ns() >= d) {
            break;
        }
    }

    let _ = ev.send(Event::Busy(true));
    let path = recording.stop()?;
    let _ = ev.send(Event::Busy(false));
    let _ = ev.send(Event::Finished {
        path,
        stats: stats.snapshot(),
        info: Box::new(info),
    });

    if shutdown {
        // Le fichier est sauve ; on peut rendre la main.
        let _ = ev.send(Event::Notice("Enregistrement finalise avant fermeture.".into()));
    }
    Ok(())
}

fn open_screen(cfg: &Config, synthetic: bool) -> Result<Box<dyn ScreenSource>> {
    if synthetic {
        let (w, h) = SYNTHETIC_SIZE;
        Ok(Box::new(crate::capture::synthetic::SyntheticScreenSource::new(
            w,
            h,
            cfg.video.fps,
            cfg.pipeline.frame_pool,
        )))
    } else {
        crate::capture::open_screen_source(cfg)
    }
}

/// Ouvre le son, ou renonce au son.
///
/// Perdre le son ne doit pas empecher d'enregistrer l'image : l'echec est
/// signale a l'interface et l'enregistrement continue muet, exactement comme
/// en ligne de commande.
fn open_audio(cfg: &Config, synthetic: bool, ev: &Sender<Event>) -> Option<Box<dyn AudioSource>> {
    if !cfg.audio.enabled {
        return None;
    }
    if synthetic {
        return Some(Box::new(crate::capture::synthetic::SyntheticAudioSource::new(
            cfg.audio.sample_rate,
            cfg.audio.channels,
            cfg.audio.fragment_ms,
        )));
    }
    match crate::capture::open_audio_source(cfg) {
        Ok(a) => Some(a),
        Err(e) => {
            let _ = ev.send(Event::Notice(format!(
                "Son indisponible ({e}) : enregistrement sans son."
            )));
            None
        }
    }
}

fn list_audio_devices() -> Result<Vec<String>> {
    #[cfg(target_os = "linux")]
    {
        crate::capture::linux::pulse::list_monitor_sources()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(crate::error::RecorderError::Unsupported(
            "liste des peripheriques audio non implementee sur cette plateforme".into(),
        ))
    }
}

fn forget_permission() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        crate::capture::linux::portal::forget_restore_token()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(crate::error::RecorderError::Unsupported(
            "sans objet sur cette plateforme".into(),
        ))
    }
}

/// Message d'erreur complet, causes comprises.
///
/// Une fenetre n'a pas de journal sous les yeux : si la cause profonde n'est
/// pas dans le texte affiche, elle est perdue pour l'utilisateur.
fn chain(e: &crate::error::RecorderError) -> String {
    let mut out = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        out.push_str(&format!("\n  cause : {cause}"));
        source = cause.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HardwarePolicy, Quality};
    use std::time::Instant;

    /// Encodeur logiciel impose, comme dans les tests d'integration : le
    /// resultat ne doit pas dependre du GPU de la machine qui execute la suite.
    fn config(path: PathBuf) -> Config {
        let mut cfg = Config::default();
        cfg.output.path = path;
        cfg.video.fps = 30;
        cfg.video.encoder = "libx264".into();
        cfg.video.hardware = HardwarePolicy::Off;
        cfg.video.quality = Quality::Low;
        cfg.video.preset = "ultrafast".into();
        cfg.audio.bitrate = 96_000;
        cfg
    }

    /// Consomme les evenements jusqu'a ce que `f` rende une valeur, ou echoue.
    fn wait_for<T>(
        controller: &Controller,
        mut f: impl FnMut(Event) -> Option<T>,
        what: &str,
    ) -> T {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            for event in controller.drain() {
                if let Event::Failed(message) = &event {
                    panic!("echec inattendu en attendant {what} : {message}");
                }
                if let Some(value) = f(event) {
                    return value;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("{what} n'est jamais arrive");
    }

    /// Le chemin exact que suit l'interface graphique, sans interface : memes
    /// messages, meme thread, meme pipeline. Seules les sources sont
    /// synthetiques, le portail exigeant un consentement interactif.
    #[test]
    fn the_controller_records_then_finalises_the_file() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let output = dir.path().join("controleur.mp4");
        let controller = Controller::spawn();
        controller.send(Command::Start(Box::new(StartRequest {
            cfg: config(output.clone()),
            synthetic_video: true,
            synthetic_audio: true,
            duration: Some(1.0),
        })));

        let info = wait_for(
            &controller,
            |e| match e {
                Event::Started(info) => Some(info),
                _ => None,
            },
            "la description de l'enregistrement",
        );
        assert_eq!(info.output, output);
        assert_eq!(info.fps, 30);

        // L'arret est demande par l'echeance, pas par l'interface : c'est le
        // comportement du reglage « arret automatique ».
        let (path, stats) = wait_for(
            &controller,
            |e| match e {
                Event::Finished { path, stats, .. } => Some((path, stats)),
                _ => None,
            },
            "la fin de l'enregistrement",
        );

        assert_eq!(path, output);
        assert!(stats.frames_encoded > 0, "aucune image encodee");
        assert!(
            path.metadata().expect("le fichier doit exister").len() > 0,
            "le fichier doit etre finalise, pas seulement cree"
        );
    }

    /// Un arret demande a la main doit finaliser le fichier aussi surement
    /// qu'une echeance : c'est le bouton « Arreter » de la fenetre.
    #[test]
    fn a_manual_stop_also_finalises_the_file() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let output = dir.path().join("arret-manuel.mp4");
        let controller = Controller::spawn();
        controller.send(Command::Start(Box::new(StartRequest {
            cfg: config(output.clone()),
            synthetic_video: true,
            synthetic_audio: true,
            duration: None,
        })));

        wait_for(
            &controller,
            |e| matches!(e, Event::Started(_)).then_some(()),
            "le demarrage",
        );
        // Laisser passer quelques images avant d'arreter.
        thread::sleep(Duration::from_millis(300));
        controller.send(Command::Stop);

        let path = wait_for(
            &controller,
            |e| match e {
                Event::Finished { path, .. } => Some(path),
                _ => None,
            },
            "la finalisation",
        );
        assert_eq!(path, output);
        assert!(path.metadata().expect("le fichier doit exister").len() > 0);
    }

    /// Une configuration refusee doit revenir comme un echec lisible, sans
    /// qu'aucune ressource n'ait ete ouverte.
    #[test]
    fn a_refused_configuration_comes_back_as_a_failure() {
        let mut cfg = Config::default();
        // Sous le minimum du projet : `validate` doit le refuser.
        cfg.video.fps = 10;
        let controller = Controller::spawn();
        controller.send(Command::Start(Box::new(StartRequest {
            cfg,
            synthetic_video: true,
            synthetic_audio: true,
            duration: Some(1.0),
        })));

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut message = None;
        while Instant::now() < deadline && message.is_none() {
            for event in controller.drain() {
                match event {
                    Event::Failed(m) => message = Some(m),
                    Event::Started(_) => panic!("une configuration refusee ne doit rien demarrer"),
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        let message = message.expect("l'echec doit etre rapporte");
        assert!(
            message.contains("fps"),
            "le message doit designer le reglage fautif : {message}"
        );
    }
}
