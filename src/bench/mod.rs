//! Mode `--benchmark` : mesurer, pas estimer.
//!
//! Le benchmark repond a une seule question : **cette machine tient-elle la
//! cadence demandee, et si non, pourquoi ?** Il execute le pipeline complet —
//! meme capture, meme encodeur, meme muxer, meme ecriture disque — pendant une
//! duree donnee, puis rend des chiffres bruts.
//!
//! La source peut etre l'ecran reel ou un generateur synthetique. Le
//! generateur a un interet precis : il produit exactement la cadence demandee,
//! ce qui isole l'encodeur et le disque du comportement du compositeur. Si le
//! benchmark synthetique tient 60 FPS mais pas le reel, le probleme vient de
//! la capture, pas de l'encodage.

use std::path::PathBuf;
use std::time::Duration;

use crate::capture::synthetic::{SyntheticAudioSource, SyntheticScreenSource};
use crate::capture::{AudioSource, ScreenSource};
use crate::config::Config;
use crate::error::Result;
use crate::performance::{gpu_busy_percent, resident_memory_bytes, CpuSampler, StatsSnapshot};
use crate::pipeline::{Recording, RecordingInfo};
use crate::timing::{monotonic_ns, NS_PER_SEC};
use crate::ui::{format_bitrate, format_bytes};

/// Reglages du banc d'essai.
#[derive(Debug, Clone)]
pub struct BenchOptions {
    pub duration: Duration,
    /// Utiliser le generateur d'images plutot que l'ecran reel.
    pub synthetic_video: bool,
    /// Utiliser le generateur audio plutot que le peripherique reel.
    ///
    /// Independant du precedent : mesurer une video synthetique avec l'audio
    /// reel est le bon compromis pour un banc d'essai non interactif, car la
    /// derive A/V mesuree reste celle d'une vraie horloge de carte son.
    pub synthetic_audio: bool,
    /// Resolution du generateur d'images.
    pub synthetic_size: (u32, u32),
}

impl Default for BenchOptions {
    fn default() -> Self {
        Self {
            duration: Duration::from_secs(10),
            synthetic_video: true,
            synthetic_audio: true,
            synthetic_size: (2560, 1440),
        }
    }
}

/// Resultat chiffre d'un banc d'essai.
#[derive(Debug, Clone)]
pub struct BenchReport {
    pub info: RecordingInfo,
    pub snapshot: StatsSnapshot,
    pub wall_secs: f64,
    pub cpu_cores: f64,
    pub cpu_percent_machine: f64,
    pub gpu_percent: Option<f64>,
    pub peak_memory: u64,
    pub output: PathBuf,
    pub output_size: u64,
    /// Latence propre a chaque branche (video, audio), en nanosecondes.
    pub leads_ns: (i64, i64),
}

impl BenchReport {
    pub fn capture_fps(&self) -> f64 {
        self.snapshot.frames_captured as f64 / self.wall_secs.max(1e-9)
    }

    pub fn encode_fps(&self) -> f64 {
        self.snapshot.frames_encoded as f64 / self.wall_secs.max(1e-9)
    }

    pub fn disk_bytes_per_sec(&self) -> f64 {
        self.snapshot.bytes_written as f64 / self.wall_secs.max(1e-9)
    }

    /// Vrai si la cadence demandee a ete reellement tenue.
    ///
    /// On exige 99 % de la cible **et** aucune image perdue : produire un
    /// fichier etiquete 60 FPS en sautant des images ne compte pas.
    pub fn target_sustained(&self) -> bool {
        let target = self.info.fps as f64;
        self.encode_fps() >= target * 0.99 && self.snapshot.frames_lost() == 0
    }

    pub fn to_text(&self) -> String {
        use std::fmt::Write as _;
        let s = &self.snapshot;
        let mut out = String::new();
        let _ = writeln!(out, "\n=== BANC D'ESSAI ===\n");
        let _ = writeln!(out, "Machine");
        let _ = writeln!(out, "  CPU              : {}", self.info.system.cpu);
        let _ = writeln!(
            out,
            "  Fils d'execution : {}",
            self.info.system.cpu_threads
        );
        for gpu in &self.info.system.gpus {
            let _ = writeln!(out, "  GPU              : {gpu}");
        }
        let _ = writeln!(out, "\nConfiguration");
        let _ = writeln!(
            out,
            "  Source           : {}",
            self.info.display
        );
        let _ = writeln!(
            out,
            "  Encodeur         : {} ({})",
            self.info.encoder_name,
            if self.info.hardware {
                "materiel"
            } else {
                "logiciel"
            }
        );
        let _ = writeln!(
            out,
            "  Cible            : {} FPS, {}",
            self.info.fps,
            format_bitrate(self.info.bitrate as f64)
        );
        let _ = writeln!(out, "  Duree mesuree    : {:.2} s", self.wall_secs);

        let _ = writeln!(out, "\nDebit");
        let _ = writeln!(out, "  FPS capture      : {:.2}", self.capture_fps());
        let _ = writeln!(out, "  FPS encodage     : {:.2}", self.encode_fps());
        let _ = writeln!(out, "  Images encodees  : {}", s.frames_encoded);
        let _ = writeln!(out, "  Images repetees  : {}", s.frames_duplicated);
        let _ = writeln!(out, "  Images fusionnees: {}", s.frames_coalesced);

        let _ = writeln!(out, "\nPertes");
        let _ = writeln!(out, "  Total perdues    : {}", s.frames_lost());
        let _ = writeln!(out, "    file saturee   : {}", s.frames_dropped_queue);
        let _ = writeln!(out, "    arrivees tard  : {}", s.frames_late);
        let _ = writeln!(out, "    slots sautes   : {}", s.slots_skipped);

        let _ = writeln!(out, "\nLatences (ms)");
        let lat = |name: &str, l: &crate::performance::LatencySummary, out: &mut String| {
            let _ = writeln!(
                out,
                "  {name:<16} : moy {:>7.3}  p50 {:>7.3}  p99 {:>7.3}  max {:>7.3}",
                l.mean_us / 1000.0,
                l.p50_us as f64 / 1000.0,
                l.p99_us as f64 / 1000.0,
                l.max_us as f64 / 1000.0
            );
        };
        lat("capture", &s.capture_latency, &mut out);
        lat("encodage", &s.encode_latency, &mut out);
        lat("audio", &s.audio_latency, &mut out);
        lat("ecriture disque", &s.disk_latency, &mut out);

        let _ = writeln!(out, "\nRessources");
        let _ = writeln!(
            out,
            "  CPU              : {:.2} coeurs ({:.1} % de la machine)",
            self.cpu_cores, self.cpu_percent_machine
        );
        let _ = writeln!(
            out,
            "  GPU              : {}",
            match self.gpu_percent {
                Some(g) => format!("{g:.1} %"),
                None => "non expose par le pilote".into(),
            }
        );
        let _ = writeln!(
            out,
            "  Memoire (pic)    : {}",
            format_bytes(self.peak_memory)
        );

        let _ = writeln!(out, "\nSortie");
        let _ = writeln!(out, "  Fichier          : {}", self.output.display());
        let _ = writeln!(out, "  Taille           : {}", format_bytes(self.output_size));
        let _ = writeln!(
            out,
            "  Debit disque     : {}/s",
            format_bytes(self.disk_bytes_per_sec() as u64)
        );
        let _ = writeln!(
            out,
            "  Debit video reel : {}",
            format_bitrate(self.disk_bytes_per_sec() * 8.0)
        );
        let _ = writeln!(out, "  Derive A/V       : {:+.2} ms", s.av_drift_ms());
        let _ = writeln!(
            out,
            "  Latence branche  : video {:.1} ms, audio {:.1} ms",
            -self.leads_ns.0 as f64 / 1e6,
            -self.leads_ns.1 as f64 / 1e6
        );

        let _ = writeln!(out, "\nSurcharges");
        let _ = writeln!(out, "  capture_overrun  : {}", s.overruns.capture_overrun);
        let _ = writeln!(out, "  encoder_overload : {}", s.overruns.encoder_overload);
        let _ = writeln!(out, "  audio_overrun    : {}", s.overruns.audio_overrun);
        let _ = writeln!(out, "  disk_write_lag   : {}", s.overruns.disk_write_lag);

        let _ = writeln!(out, "\nVERDICT");
        if self.target_sustained() {
            let _ = writeln!(
                out,
                "  ✓ {} FPS tenus de bout en bout, sans perte.",
                self.info.fps
            );
        } else {
            let _ = writeln!(
                out,
                "  ✗ {} FPS NON tenus : {:.2} FPS reels, {} images perdues.",
                self.info.fps,
                self.encode_fps(),
                s.frames_lost()
            );
            let _ = writeln!(out, "{}", self.diagnosis());
        }
        out
    }

    /// Designe le maillon faible a partir des compteurs.
    fn diagnosis(&self) -> String {
        let s = &self.snapshot;
        let mut lines = Vec::new();
        if s.overruns.encoder_overload > 0 || s.slots_skipped > 0 {
            lines.push(format!(
                "  → L'encodage est le goulot d'etranglement (p99 = {:.1} ms pour un budget de {:.1} ms).",
                s.encode_latency.p99_us as f64 / 1000.0,
                1000.0 / self.info.fps as f64
            ));
            if !self.info.hardware {
                lines.push(
                    "    L'encodeur est logiciel : verifiez la disponibilite de VAAPI/NVENC/QSV."
                        .into(),
                );
            } else {
                lines.push(
                    "    Essayez une qualite inferieure, un preset plus rapide, ou 30 FPS.".into(),
                );
            }
        }
        if s.overruns.capture_overrun > 0 {
            lines.push(format!(
                "  → La file de capture a deborde {} fois : l'aval ne consomme pas assez vite.",
                s.overruns.capture_overrun
            ));
        }
        if s.overruns.disk_write_lag > 0 {
            lines.push(format!(
                "  → Le disque a decroche {} fois (p99 = {:.1} ms).",
                s.overruns.disk_write_lag,
                s.disk_latency.p99_us as f64 / 1000.0
            ));
        }
        if self.capture_fps() < self.info.fps as f64 * 0.95 && s.frames_lost() == 0 {
            lines.push(format!(
                "  → La source n'a fourni que {:.2} FPS : la limite est en amont de l'encodeur.",
                self.capture_fps()
            ));
        }
        if lines.is_empty() {
            lines.push("  → Aucun compteur de surcharge : la source elle-meme est limitante.".into());
        }
        lines.join("\n")
    }
}

/// Execute le banc d'essai.
pub fn run(cfg: &Config, opts: &BenchOptions) -> Result<BenchReport> {
    let screen: Box<dyn ScreenSource> = if opts.synthetic_video {
        let (w, h) = opts.synthetic_size;
        Box::new(SyntheticScreenSource::new(
            w,
            h,
            cfg.video.fps,
            cfg.pipeline.frame_pool,
        ))
    } else {
        crate::capture::open_screen_source(cfg)?
    };

    let audio: Option<Box<dyn AudioSource>> = if !cfg.audio.enabled {
        None
    } else if opts.synthetic_audio {
        Some(Box::new(SyntheticAudioSource::new(
            cfg.audio.sample_rate,
            cfg.audio.channels,
            cfg.audio.fragment_ms,
        )))
    } else {
        match crate::capture::open_audio_source(cfg) {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!(erreur = %e, "banc d'essai sans audio");
                None
            }
        }
    };

    let recording = Recording::start(cfg, screen, audio)?;
    let info = recording.info().clone();

    // Echantillonnage des ressources pendant la mesure. Le premier appel a
    // `CpuSampler::sample` sert de reference et est donc ignore.
    let mut cpu = CpuSampler::new();
    let _ = cpu.sample();
    let mut peak_memory = resident_memory_bytes().unwrap_or(0);
    let mut gpu_samples: Vec<f64> = Vec::new();

    let started = monotonic_ns();
    let deadline = started + opts.duration.as_nanos() as i64;
    while monotonic_ns() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        if let Some(mem) = resident_memory_bytes() {
            peak_memory = peak_memory.max(mem);
        }
        if let Some(g) = gpu_busy_percent() {
            gpu_samples.push(g);
        }
    }
    let wall_secs = (monotonic_ns() - started) as f64 / NS_PER_SEC as f64;

    let snapshot = recording.stats().snapshot();
    let leads_ns = recording.stats().leads_ns();
    let usage = cpu.sample();
    let output = recording.stop()?;
    let output_size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);

    Ok(BenchReport {
        info,
        snapshot,
        wall_secs,
        cpu_cores: usage.map_or(0.0, |u| u.cores),
        cpu_percent_machine: usage.map_or(0.0, |u| u.percent_of_machine),
        gpu_percent: if gpu_samples.is_empty() {
            None
        } else {
            Some(gpu_samples.iter().sum::<f64>() / gpu_samples.len() as f64)
        },
        peak_memory,
        output,
        output_size,
        leads_ns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HardwarePolicy;

    fn bench_config(path: PathBuf) -> Config {
        let mut cfg = Config::default();
        cfg.output.path = path;
        cfg.video.fps = 30;
        cfg.video.encoder = "libx264".into();
        cfg.video.hardware = HardwarePolicy::Off;
        cfg.video.quality = crate::config::Quality::Low;
        cfg.audio.enabled = false;
        cfg
    }

    #[test]
    fn a_short_benchmark_produces_a_complete_report() {
        let dir = tempfile::tempdir().expect("dossier");
        let cfg = bench_config(dir.path().join("bench.mp4"));
        let opts = BenchOptions {
            duration: Duration::from_millis(1200),
            synthetic_video: true,
            synthetic_audio: true,
            synthetic_size: (320, 240),
        };
        let report = run(&cfg, &opts).expect("banc d'essai");
        assert!(report.wall_secs >= 1.0);
        assert!(report.encode_fps() > 5.0, "{}", report.encode_fps());
        assert!(report.output_size > 0);

        let text = report.to_text();
        for expected in [
            "BANC D'ESSAI",
            "FPS encodage",
            "Latences (ms)",
            "p99",
            "Ressources",
            "Derive A/V",
            "VERDICT",
        ] {
            assert!(text.contains(expected), "section absente : {expected}\n{text}");
        }
    }

    #[test]
    fn the_verdict_requires_both_the_rate_and_zero_loss() {
        let dir = tempfile::tempdir().expect("dossier");
        let cfg = bench_config(dir.path().join("verdict.mp4"));
        let opts = BenchOptions {
            duration: Duration::from_millis(900),
            synthetic_video: true,
            synthetic_audio: true,
            synthetic_size: (160, 120),
        };
        let mut report = run(&cfg, &opts).expect("banc d'essai");

        // Cadence atteinte mais images perdues : le verdict doit rester negatif.
        report.snapshot.frames_encoded = (30.0 * report.wall_secs) as u64;
        report.snapshot.slots_skipped = 5;
        assert!(!report.target_sustained());
        assert!(report.to_text().contains("NON tenus"));

        report.snapshot.slots_skipped = 0;
        assert!(report.target_sustained());
    }

    #[test]
    fn the_diagnosis_names_the_bottleneck() {
        let dir = tempfile::tempdir().expect("dossier");
        let cfg = bench_config(dir.path().join("diag.mp4"));
        let opts = BenchOptions {
            duration: Duration::from_millis(900),
            synthetic_video: true,
            synthetic_audio: true,
            synthetic_size: (160, 120),
        };
        let mut report = run(&cfg, &opts).expect("banc d'essai");
        report.snapshot.slots_skipped = 100;
        report.snapshot.overruns.encoder_overload = 4;
        report.snapshot.frames_encoded = 1;
        let text = report.to_text();
        assert!(text.contains("goulot d'etranglement"), "{text}");
        assert!(text.contains("logiciel"), "{text}");
    }
}
