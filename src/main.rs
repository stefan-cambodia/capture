//! `rscap` — enregistreur d'ecran temps reel.
//!
//! Le binaire ne contient que l'analyse de la ligne de commande, la mise en
//! place des journaux et la boucle d'affichage. Toute la logique est dans la
//! bibliotheque, ce qui permet de la tester sans passer par le CLI.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

use rscap::bench::{self, BenchOptions};
use rscap::capture::{AudioSource, ScreenSource};
use rscap::config::{
    Config, ContainerFormat, HardwarePolicy, Pacing, Quality, RateControl, VideoCodec,
};
use rscap::error::{RecorderError, Result};
use rscap::pipeline::Recording;
use rscap::timing::monotonic_ns;
use rscap::ui;

/// Periode de rafraichissement du tableau de bord.
const REFRESH: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliCodec {
    H264,
    Hevc,
    Av1,
}

impl From<CliCodec> for VideoCodec {
    fn from(c: CliCodec) -> Self {
        match c {
            CliCodec::H264 => VideoCodec::H264,
            CliCodec::Hevc => VideoCodec::Hevc,
            CliCodec::Av1 => VideoCodec::Av1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliQuality {
    Low,
    Medium,
    High,
    VeryHigh,
}

impl From<CliQuality> for Quality {
    fn from(q: CliQuality) -> Self {
        match q {
            CliQuality::Low => Quality::Low,
            CliQuality::Medium => Quality::Medium,
            CliQuality::High => Quality::High,
            CliQuality::VeryHigh => Quality::VeryHigh,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliHardware {
    Auto,
    Force,
    Off,
}

impl From<CliHardware> for HardwarePolicy {
    fn from(h: CliHardware) -> Self {
        match h {
            CliHardware::Auto => HardwarePolicy::Auto,
            CliHardware::Force => HardwarePolicy::Force,
            CliHardware::Off => HardwarePolicy::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliContainer {
    Mp4,
    Mkv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliRateControl {
    Vbr,
    Cbr,
    Cq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliPacing {
    Cfr,
    Vfr,
}

#[derive(Debug, Parser)]
#[command(
    name = "rscap",
    about = "Enregistreur d'ecran temps reel : capture plein ecran avec son systeme",
    long_about = None,
    version
)]
struct Cli {
    /// Fichier de configuration TOML.
    #[arg(short, long, value_name = "FICHIER")]
    config: Option<PathBuf>,

    /// Fichier de sortie.
    #[arg(short, long, value_name = "CHEMIN")]
    output: Option<PathBuf>,

    /// Images par seconde visees (30 minimum).
    #[arg(long)]
    fps: Option<u32>,

    #[arg(long, value_enum)]
    codec: Option<CliCodec>,

    #[arg(long, value_enum)]
    quality: Option<CliQuality>,

    /// Debit video en bits par seconde (0 = automatique).
    #[arg(long)]
    bitrate: Option<u64>,

    #[arg(long, value_enum)]
    rate_control: Option<CliRateControl>,

    /// Politique d'encodage materiel.
    #[arg(long, value_enum)]
    hardware: Option<CliHardware>,

    /// Force un encodeur precis (ex. h264_vaapi, libx264).
    #[arg(long, value_name = "NOM")]
    encoder: Option<String>,

    #[arg(long, value_enum)]
    container: Option<CliContainer>,

    /// Cadencement du flux de sortie.
    #[arg(long, value_enum)]
    pacing: Option<CliPacing>,

    /// Desactive la capture du son systeme.
    #[arg(long)]
    no_audio: bool,

    /// Peripherique audio (moniteur de sortie). Vide = defaut du systeme.
    #[arg(long, value_name = "NOM")]
    audio_device: Option<String>,

    /// Arret automatique apres N secondes.
    #[arg(short, long, value_name = "SECONDES")]
    duration: Option<f64>,

    /// Utilise le generateur d'images au lieu de l'ecran reel.
    ///
    /// N'affecte que la video : le son systeme reste capture normalement, ce
    /// qui permet de mesurer une vraie derive A/V sans ouvrir la boite de
    /// dialogue du portail.
    #[arg(long)]
    synthetic: bool,

    /// Utilise aussi le generateur audio (mesure entierement deterministe).
    #[arg(long)]
    synthetic_audio: bool,

    /// Lance le banc d'essai au lieu d'un enregistrement.
    #[arg(long)]
    benchmark: bool,

    /// Duree du banc d'essai, en secondes.
    #[arg(long, default_value_t = 10.0, value_name = "SECONDES")]
    benchmark_duration: f64,

    /// Affiche le materiel detecte et les encodeurs disponibles.
    #[arg(long)]
    probe: bool,

    /// Liste les moniteurs de sortie audio.
    #[arg(long)]
    list_audio: bool,

    /// Affiche la configuration effective en TOML puis quitte.
    #[arg(long)]
    print_config: bool,

    /// Oublie l'autorisation memorisee du portail (redemande l'ecran).
    #[arg(long)]
    forget_permission: bool,

    /// Verbosite : -v pour info, -vv pour debug, -vvv pour trace.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Ecrit les journaux dans un fichier plutot que sur la sortie d'erreur.
    #[arg(long, value_name = "CHEMIN")]
    log_file: Option<PathBuf>,
}

impl Cli {
    /// Applique les options a une configuration de base.
    fn apply(&self, cfg: &mut Config) {
        if let Some(v) = &self.output {
            cfg.output.path = v.clone();
        }
        if let Some(v) = self.fps {
            cfg.video.fps = v;
        }
        if let Some(v) = self.codec {
            cfg.video.codec = v.into();
        }
        if let Some(v) = self.quality {
            cfg.video.quality = v.into();
        }
        if let Some(v) = self.bitrate {
            cfg.video.bitrate = v;
        }
        if let Some(v) = self.rate_control {
            cfg.video.rate_control = match v {
                CliRateControl::Vbr => RateControl::Vbr,
                CliRateControl::Cbr => RateControl::Cbr,
                CliRateControl::Cq => RateControl::Cq,
            };
        }
        if let Some(v) = self.hardware {
            cfg.video.hardware = v.into();
        }
        if let Some(v) = &self.encoder {
            cfg.video.encoder = v.clone();
        }
        if let Some(v) = self.container {
            cfg.output.format = match v {
                CliContainer::Mp4 => ContainerFormat::Mp4,
                CliContainer::Mkv => ContainerFormat::Mkv,
            };
        }
        if let Some(v) = self.pacing {
            cfg.video.pacing = match v {
                CliPacing::Cfr => Pacing::Cfr,
                CliPacing::Vfr => Pacing::Vfr,
            };
        }
        if self.no_audio {
            cfg.audio.enabled = false;
        }
        if let Some(v) = &self.audio_device {
            cfg.audio.device = v.clone();
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let _log_guard = init_logging(&cli);

    if let Err(e) = run(&cli) {
        // Une erreur doit etre lisible sans avoir a lire les journaux.
        eprintln!("\nErreur : {e}");
        let mut source = std::error::Error::source(&e);
        while let Some(cause) = source {
            eprintln!("  cause : {cause}");
            source = cause.source();
        }
        std::process::exit(1);
    }
}

fn init_logging(cli: &Cli) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let level = match cli.verbose {
        0 => "rscap=warn",
        1 => "rscap=info",
        2 => "rscap=debug",
        _ => "rscap=trace",
    };
    let filter = EnvFilter::try_from_env("RSCAP_LOG").unwrap_or_else(|_| EnvFilter::new(level));

    // Ecriture non bloquante : un journal ne doit jamais faire attendre un
    // thread temps reel, meme si la sortie est un terminal lent ou un tube
    // sature.
    let (writer, guard) = match &cli.log_file {
        Some(path) => match std::fs::File::create(path) {
            Ok(f) => tracing_appender::non_blocking(f),
            Err(e) => {
                eprintln!("journal impossible dans {} : {e}", path.display());
                tracing_appender::non_blocking(std::io::stderr())
            }
        },
        None => tracing_appender::non_blocking(std::io::stderr()),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_target(false)
                .with_ansi(cli.log_file.is_none() && ui::is_tty()),
        )
        .init();
    Some(guard)
}

fn run(cli: &Cli) -> Result<()> {
    // --- actions qui ne demarrent pas d'enregistrement ---
    if cli.forget_permission {
        #[cfg(target_os = "linux")]
        {
            rscap::capture::linux::portal::forget_restore_token()?;
            println!("Autorisation du portail oubliee : l'ecran sera redemande.");
        }
        #[cfg(not(target_os = "linux"))]
        println!("Sans objet sur cette plateforme.");
        return Ok(());
    }

    if cli.list_audio {
        return list_audio();
    }

    let mut cfg = match &cli.config {
        Some(path) => Config::from_toml_file(path)?,
        None => Config::default(),
    };
    cli.apply(&mut cfg);
    cfg.normalize_output_extension();
    cfg.validate()?;

    if cli.print_config {
        print!("{}", cfg.to_toml()?);
        return Ok(());
    }

    if cli.probe {
        return probe(&cfg);
    }

    if cli.benchmark {
        return run_benchmark(cli, &cfg);
    }

    record(cli, &cfg)
}

fn list_audio() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let monitors = rscap::capture::linux::pulse::list_monitor_sources()?;
        if monitors.is_empty() {
            println!("Aucun moniteur de sortie trouve.");
            println!("Verifiez que PipeWire ou PulseAudio est demarre.");
        } else {
            println!("Moniteurs de sortie (son systeme) :");
            for m in monitors {
                println!("  {m}");
            }
            println!("\nUtilisez --audio-device <nom> pour en choisir un.");
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    Err(RecorderError::Unsupported(
        "liste des peripheriques audio non implementee sur cette plateforme".into(),
    ))
}

fn probe(cfg: &Config) -> Result<()> {
    ffmpeg_next::init().map_err(|e| RecorderError::PipelineAborted(format!("ffmpeg : {e}")))?;
    let system = rscap::encoder::hwdetect::detect();

    println!("Materiel");
    println!("  CPU              : {}", system.cpu);
    println!("  Fils d'execution : {}", system.cpu_threads);
    if system.gpus.is_empty() {
        println!("  GPU              : aucun detecte");
    }
    for gpu in &system.gpus {
        println!("  GPU              : {gpu}");
    }
    #[cfg(target_os = "linux")]
    println!("  Session          : {}", rscap::capture::linux::session_type());

    println!("\nEncodeurs candidats pour {} (ordre d'essai)", cfg.video.codec);
    let candidates = rscap::encoder::hwdetect::candidates(
        cfg.video.codec,
        cfg.video.hardware,
        system.vendor(),
        &cfg.video.encoder,
    );
    for cand in &candidates {
        let present = ffmpeg_next::encoder::find_by_name(cand.name).is_some();
        println!(
            "  {:<14} {:<10} {}",
            cand.name,
            if cand.accel.is_hardware() {
                "materiel"
            } else {
                "logiciel"
            },
            if present {
                "present dans ffmpeg"
            } else {
                "ABSENT de cette build"
            }
        );
    }

    // Seule l'ouverture reelle prouve qu'un encodeur fonctionne.
    println!("\nOuverture reelle a {}x{}", 1920, 1080);
    let mut spec_cfg = cfg.video.clone();
    spec_cfg.encoder = String::new();
    match rscap::encoder::video::VideoEncoder::open(&rscap::encoder::video::VideoEncoderSpec {
        width: 1920,
        height: 1080,
        cfg: spec_cfg,
        global_header: true,
        system: system.clone(),
    }) {
        Ok(enc) => println!(
            "  retenu : {} ({}, {})",
            enc.name(),
            enc.codec(),
            if enc.is_hardware() {
                "materiel"
            } else {
                "logiciel"
            }
        ),
        Err(e) => println!("  aucun encodeur utilisable : {e}"),
    }
    Ok(())
}

/// Sources ouvertes, pretes a etre confiees au pipeline.
type Sources = (Box<dyn ScreenSource>, Option<Box<dyn AudioSource>>);

fn open_sources(cli: &Cli, cfg: &Config) -> Result<Sources> {
    let screen: Box<dyn ScreenSource> = if cli.synthetic {
        Box::new(rscap::capture::synthetic::SyntheticScreenSource::new(
            2560,
            1440,
            cfg.video.fps,
            cfg.pipeline.frame_pool,
        ))
    } else {
        rscap::capture::open_screen_source(cfg)?
    };

    let audio: Option<Box<dyn AudioSource>> = if !cfg.audio.enabled {
        None
    } else if cli.synthetic_audio {
        Some(Box::new(
            rscap::capture::synthetic::SyntheticAudioSource::new(
                cfg.audio.sample_rate,
                cfg.audio.channels,
                cfg.audio.fragment_ms,
            ),
        ))
    } else {
        match rscap::capture::open_audio_source(cfg) {
            Ok(a) => Some(a),
            Err(e) => {
                // Perdre le son ne doit pas empecher d'enregistrer l'image.
                eprintln!("Avertissement : capture audio indisponible ({e})");
                eprintln!("L'enregistrement continue sans son.");
                None
            }
        }
    };
    Ok((screen, audio))
}

fn record(cli: &Cli, cfg: &Config) -> Result<()> {
    let (screen, audio) = open_sources(cli, cfg)?;
    let recording = Recording::start(cfg, screen, audio)?;
    let info = recording.info().clone();

    print!("{}", ui::banner(&info));
    println!("\nAppuyez sur Ctrl+C pour arreter.\n");

    // On garde une reference aux compteurs : `stop()` consomme
    // l'enregistrement, mais le resume doit refleter l'etat *final*, taille du
    // fichier finalise comprise.
    let stats = Arc::clone(recording.stats());
    let interrupted = install_signal_handler()?;
    let deadline = cli
        .duration
        .map(|d| monotonic_ns() + (d * 1e9) as i64);

    let mut view = ui::StatusView::new();
    loop {
        std::thread::sleep(REFRESH);
        let snapshot = stats.snapshot();
        view.draw(&snapshot, &info);

        if interrupted.load(Ordering::Relaxed) {
            break;
        }
        if deadline.is_some_and(|d| monotonic_ns() >= d) {
            break;
        }
    }
    view.finish();
    println!("Finalisation du fichier...");

    let path = recording.stop()?;
    // Photo prise apres l'arret : elle inclut la finalisation du conteneur.
    print!("{}", ui::summary(&stats.snapshot(), &info, &path));
    Ok(())
}

fn run_benchmark(cli: &Cli, cfg: &Config) -> Result<()> {
    let opts = BenchOptions {
        duration: Duration::from_secs_f64(cli.benchmark_duration.clamp(1.0, 3600.0)),
        // Par defaut on mesure la chaine reelle ; `--synthetic` isole
        // l'encodeur et le disque du comportement du compositeur.
        synthetic_video: cli.synthetic,
        synthetic_audio: cli.synthetic_audio,
        synthetic_size: (2560, 1440),
    };
    println!(
        "Banc d'essai : {:.0} s, source {}, cible {} FPS",
        opts.duration.as_secs_f64(),
        if opts.synthetic_video {
            "synthetique"
        } else {
            "ecran reel"
        },
        cfg.video.fps
    );
    let report = bench::run(cfg, &opts)?;
    print!("{}", report.to_text());
    if !report.target_sustained() {
        // Code de sortie distinct : utilisable dans un script.
        std::process::exit(2);
    }
    Ok(())
}

fn install_signal_handler() -> Result<Arc<AtomicBool>> {
    let flag = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&flag);
    ctrlc::set_handler(move || {
        handler_flag.store(true, Ordering::Relaxed);
    })
    .map_err(|e| RecorderError::PipelineAborted(format!("gestionnaire de signal : {e}")))?;
    Ok(flag)
}
