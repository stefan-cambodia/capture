//! Interface terminal : banniere, tableau de bord temps reel, resume final.
//!
//! # Contrainte
//!
//! L'affichage ne doit jamais ralentir le pipeline. Il tourne sur le thread
//! principal, a 5 Hz, et ne fait que lire un [`StatsSnapshot`] — une photo de
//! compteurs atomiques. Il ne prend aucun verrou du chemin chaud et n'appelle
//! jamais le pipeline.
//!
//! # Deux modes
//!
//! Sur un terminal interactif, le tableau de bord se redessine sur place avec
//! des sequences ANSI. Rediriges vers un fichier ou un tube, les codes de
//! controle rendraient le journal illisible : on ecrit alors une ligne par
//! rafraichissement.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;

use crate::performance::{CpuSampler, Rate, StatsSnapshot};
use crate::pipeline::RecordingInfo;

/// Vrai si la sortie standard est un terminal.
pub fn is_tty() -> bool {
    // SAFETY: `isatty` ne fait que consulter un descripteur.
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

/// Formate une duree en `h:mm:ss`.
pub fn format_duration(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    format!("{}:{:02}:{:02}", total / 3600, (total / 60) % 60, total % 60)
}

/// Formate un debit binaire.
pub fn format_bitrate(bps: f64) -> String {
    if bps >= 1_000_000.0 {
        format!("{:.1} Mb/s", bps / 1_000_000.0)
    } else if bps >= 1_000.0 {
        format!("{:.0} kb/s", bps / 1_000.0)
    } else {
        format!("{bps:.0} b/s")
    }
}

/// Formate une taille en octets.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["o", "Kio", "Mio", "Gio"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} o")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Banniere affichee avant le demarrage.
pub fn banner(info: &RecordingInfo) -> String {
    let width = 56;
    let line = "─".repeat(width - 2);
    let mut out = String::new();
    let row = |s: String, out: &mut String| {
        // La largeur visuelle se compte en caracteres, pas en octets.
        let pad = width.saturating_sub(4 + s.chars().count());
        let _ = writeln!(out, "│ {}{} │", s, " ".repeat(pad));
    };

    let _ = writeln!(out, "┌{line}┐");
    row("SCREEN RECORDER".into(), &mut out);
    row(String::new(), &mut out);
    row(format!("Ecran      : {}", info.display), &mut out);
    row(
        format!("Resolution : {}x{}", info.width, info.height),
        &mut out,
    );
    row(format!("FPS cible  : {}", info.fps), &mut out);
    row(
        format!(
            "Codec      : {} ({}{})",
            info.codec,
            info.encoder_name,
            if info.hardware {
                ", materiel"
            } else {
                ", logiciel"
            }
        ),
        &mut out,
    );
    row(
        format!("Debit      : {}", format_bitrate(info.bitrate as f64)),
        &mut out,
    );
    let audio = match &info.audio {
        Some(a) => format!("{a}"),
        None => "desactive".into(),
    };
    row(format!("Audio      : {audio}"), &mut out);
    row(
        format!("Sortie     : {}", info.output.display()),
        &mut out,
    );
    let _ = writeln!(out, "└{line}┘");
    out
}

/// Tableau de bord rafraichi pendant l'enregistrement.
pub struct StatusView {
    previous: Option<StatsSnapshot>,
    cpu: CpuSampler,
    /// Lignes dessinees au tour precedent, a effacer avant de redessiner.
    drawn: usize,
    interactive: bool,
    warned_overrun: bool,
}

impl Default for StatusView {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusView {
    pub fn new() -> Self {
        Self {
            previous: None,
            cpu: CpuSampler::new(),
            drawn: 0,
            interactive: is_tty(),
            warned_overrun: false,
        }
    }

    /// Debits depuis le dernier rafraichissement.
    ///
    /// A appeler **avant** `render`, qui remplace la photo de reference.
    fn rate_since_last(&self, snap: &StatsSnapshot) -> Rate {
        self.previous
            .as_ref()
            .map(|p| Rate::between(p, snap))
            .unwrap_or_default()
    }

    /// Construit le bloc de statut. Separe de l'affichage pour etre testable.
    pub fn render(&mut self, snap: &StatsSnapshot, info: &RecordingInfo) -> String {
        let rate = self.rate_since_last(snap);
        let cpu = self.cpu.sample();
        self.previous = Some(*snap);

        let mut out = String::new();
        let _ = writeln!(
            out,
            "● Enregistrement  {}   {}",
            format_duration(snap.duration_secs()),
            format_bytes(snap.bytes_written)
        );

        // Le FPS reel est le nombre d'images *encodees* par seconde : c'est
        // ce qui finit dans le fichier, pas ce qui a ete capture.
        let fps_line = format!(
            "FPS      {:>6.2} / {}   capture {:>6.2}",
            rate.encode_fps, info.fps, rate.capture_fps
        );
        let _ = writeln!(out, "{fps_line}");

        let _ = writeln!(
            out,
            "Images   perdues {}   dupliquees {}   fusionnees {}",
            snap.frames_lost(),
            snap.frames_duplicated,
            snap.frames_coalesced
        );

        let _ = writeln!(
            out,
            "A/V      derive {:+.1} ms   audio {} trous, {} corrections",
            snap.av_drift_ms(),
            snap.audio_gaps_filled,
            snap.audio_compensations
        );

        let _ = writeln!(
            out,
            "Files    video {}/{}   audio {}/{}   paquets {}/{}",
            snap.video_queue.0,
            snap.video_queue.1,
            snap.audio_queue.0,
            snap.audio_queue.1,
            snap.packet_queue.0,
            snap.packet_queue.1
        );

        let cpu_text = match cpu {
            Some(u) => format!("{:.0} %", u.percent_of_one_core),
            None => "n/d".into(),
        };
        let gpu_text = match crate::performance::gpu_busy_percent() {
            Some(g) => format!("{g:.0} %"),
            None => "n/d".into(),
        };
        let _ = writeln!(
            out,
            "Charge   CPU {cpu_text}   GPU {gpu_text}   debit {}",
            format_bitrate(rate.bitrate_bps)
        );

        let _ = writeln!(
            out,
            "Latence  encodage {:.1} ms (p99 {:.1})   capture {:.1} ms",
            snap.encode_latency.mean_us / 1000.0,
            snap.encode_latency.p99_us as f64 / 1000.0,
            snap.capture_latency.mean_us / 1000.0,
        );

        // Un probleme de performance doit se voir, pas se deviner.
        if snap.overruns.any() {
            let _ = writeln!(
                out,
                "⚠ Surcharge : capture {}  encodeur {}  audio {}  disque {}",
                snap.overruns.capture_overrun,
                snap.overruns.encoder_overload,
                snap.overruns.audio_overrun,
                snap.overruns.disk_write_lag
            );
        }
        out
    }

    /// Dessine le bloc, en place si le terminal le permet.
    pub fn draw(&mut self, snap: &StatsSnapshot, info: &RecordingInfo) {
        // Calcule avant `render`, qui va remplacer la photo de reference.
        let rate = self.rate_since_last(snap);
        let block = self.render(snap, info);
        let mut stdout = io::stdout().lock();

        if self.interactive {
            // Remonter puis effacer : evite le clignotement d'un clear total.
            for _ in 0..self.drawn {
                let _ = write!(stdout, "\x1b[1A\x1b[2K");
            }
            self.drawn = block.lines().count();
            let _ = write!(stdout, "{block}");
        } else {
            // Journal : une ligne compacte, sans code de controle.
            let _ = writeln!(
                stdout,
                "t={} fps={:.2}/{} perdues={} dupliquees={} derive={:+.1}ms debit={}",
                format_duration(snap.duration_secs()),
                rate.encode_fps,
                info.fps,
                snap.frames_lost(),
                snap.frames_duplicated,
                snap.av_drift_ms(),
                format_bitrate(rate.bitrate_bps)
            );
        }
        let _ = stdout.flush();

        if snap.overruns.any() && !self.warned_overrun {
            self.warned_overrun = true;
            tracing::warn!(
                capture = snap.overruns.capture_overrun,
                encodeur = snap.overruns.encoder_overload,
                audio = snap.overruns.audio_overrun,
                disque = snap.overruns.disk_write_lag,
                "surcharge detectee"
            );
        }
    }

    /// Libere la zone de dessin avant d'ecrire autre chose.
    pub fn finish(&mut self) {
        if self.interactive && self.drawn > 0 {
            let mut stdout = io::stdout().lock();
            let _ = writeln!(stdout);
            let _ = stdout.flush();
        }
        self.drawn = 0;
    }
}

/// Resume affiche apres l'arret.
pub fn summary(snap: &StatsSnapshot, info: &RecordingInfo, path: &Path) -> String {
    let mut out = String::new();
    let duration = snap.duration_secs();
    let average_fps = if duration > 0.0 {
        snap.frames_encoded as f64 / duration
    } else {
        0.0
    };

    let _ = writeln!(out, "\nEnregistrement termine");
    let _ = writeln!(out, "  Fichier          : {}", path.display());
    let _ = writeln!(
        out,
        "  Taille           : {}",
        format_bytes(snap.bytes_written)
    );
    let _ = writeln!(out, "  Duree            : {}", format_duration(duration));
    let _ = writeln!(out, "  FPS cible        : {}", info.fps);
    let _ = writeln!(out, "  FPS reel moyen   : {average_fps:.2}");
    let _ = writeln!(out, "  Images encodees  : {}", snap.frames_encoded);
    let _ = writeln!(out, "  Images capturees : {}", snap.frames_captured);
    let _ = writeln!(out, "  Images perdues   : {}", snap.frames_lost());
    let _ = writeln!(out, "  Images repetees  : {}", snap.frames_duplicated);
    let _ = writeln!(
        out,
        "  Debit moyen      : {}",
        format_bitrate(if duration > 0.0 {
            snap.bytes_written as f64 * 8.0 / duration
        } else {
            0.0
        })
    );
    let _ = writeln!(out, "  Derive A/V finale: {:+.2} ms", snap.av_drift_ms());

    // Verdict franc : on ne pretend jamais avoir tenu la cadence.
    let target = info.fps as f64;
    if average_fps < target * 0.97 {
        let _ = writeln!(
            out,
            "\n  ⚠ La cadence de {target:.0} FPS n'a PAS ete tenue ({average_fps:.2} FPS reels)."
        );
        if snap.overruns.encoder_overload > 0 {
            let _ = writeln!(
                out,
                "    Cause probable : l'encodeur ne suit pas. Essayez un encodeur materiel,"
            );
            let _ = writeln!(
                out,
                "    une qualite inferieure, ou 30 FPS."
            );
        }
        if snap.overruns.capture_overrun > 0 {
            let _ = writeln!(
                out,
                "    Cause probable : la file de capture a deborde ({} fois).",
                snap.overruns.capture_overrun
            );
        }
        if snap.overruns.disk_write_lag > 0 {
            let _ = writeln!(
                out,
                "    Cause probable : le disque n'a pas suivi ({} ecritures lentes).",
                snap.overruns.disk_write_lag
            );
        }
    } else {
        let _ = writeln!(out, "\n  ✓ Cadence de {target:.0} FPS tenue.");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::DisplayInfo;
    use crate::config::VideoCodec;
    use crate::encoder::hwdetect;
    use std::path::PathBuf;

    fn info(fps: u32) -> RecordingInfo {
        RecordingInfo {
            display: DisplayInfo {
                name: "test".into(),
                width: 2560,
                height: 1440,
                refresh_mhz: Some(144_000),
                primary: true,
            },
            audio: None,
            encoder_name: "h264_vaapi",
            codec: VideoCodec::H264,
            hardware: true,
            width: 2560,
            height: 1440,
            fps,
            bitrate: 35_000_000,
            output: PathBuf::from("/tmp/out.mp4"),
            system: hwdetect::detect(),
        }
    }

    #[test]
    fn durations_are_formatted_as_hours_minutes_seconds() {
        assert_eq!(format_duration(0.0), "0:00:00");
        assert_eq!(format_duration(83.0), "0:01:23");
        assert_eq!(format_duration(3661.0), "1:01:01");
        // Une valeur negative ne doit pas produire d'affichage absurde.
        assert_eq!(format_duration(-5.0), "0:00:00");
    }

    #[test]
    fn bitrates_and_sizes_use_readable_units() {
        assert_eq!(format_bitrate(35_000_000.0), "35.0 Mb/s");
        assert_eq!(format_bitrate(128_000.0), "128 kb/s");
        assert_eq!(format_bytes(512), "512 o");
        assert_eq!(format_bytes(1536), "1.5 Kio");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 Mio");
    }

    #[test]
    fn the_banner_is_a_closed_box() {
        let text = banner(&info(60));
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with('┌') && lines[0].ends_with('┐'));
        assert!(lines[lines.len() - 1].starts_with('└'));
        // Toutes les lignes internes ont la meme largeur visuelle.
        let widths: Vec<usize> = lines.iter().map(|l| l.chars().count()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "largeurs inegales : {widths:?}"
        );
        assert!(text.contains("2560x1440"));
        assert!(text.contains("h264_vaapi"));
    }

    #[test]
    fn the_dashboard_shows_the_real_fps_not_the_target() {
        let stats = crate::performance::Stats::new();
        stats.mark_start(crate::timing::monotonic_ns());
        let mut view = StatusView::new();
        let a = stats.snapshot();
        let _ = view.render(&a, &info(60));

        stats.frames_encoded(45);
        let mut b = stats.snapshot();
        b.at_ns = a.at_ns + crate::timing::NS_PER_SEC;
        let text = view.render(&b, &info(60));
        assert!(text.contains("45.00 / 60"), "{text}");
    }

    #[test]
    fn the_log_line_reports_the_same_fps_as_the_dashboard() {
        // Regression : la ligne de journal calculait son debit apres que
        // `render` ait deja avance la photo de reference, et affichait donc
        // toujours 0.00 FPS.
        let stats = crate::performance::Stats::new();
        stats.mark_start(crate::timing::monotonic_ns());
        let view = StatusView::new();
        let a = stats.snapshot();
        assert_eq!(view.rate_since_last(&a).encode_fps, 0.0);

        let mut view = StatusView::new();
        let _ = view.render(&a, &info(60));
        stats.frames_encoded(30);
        let mut b = stats.snapshot();
        b.at_ns = a.at_ns + crate::timing::NS_PER_SEC;
        let rate = view.rate_since_last(&b);
        assert!((rate.encode_fps - 30.0).abs() < 1e-9, "{}", rate.encode_fps);
    }

    #[test]
    fn an_overrun_is_shown_not_hidden() {
        let stats = crate::performance::Stats::new();
        stats.encoder_overload(3);
        let mut view = StatusView::new();
        let text = view.render(&stats.snapshot(), &info(60));
        assert!(text.contains("Surcharge"), "{text}");
    }

    #[test]
    fn the_summary_states_plainly_when_the_target_was_missed() {
        let stats = crate::performance::Stats::new();
        stats.frames_encoded(100);
        stats.encoder_overload(5);
        let mut snap = stats.snapshot();
        snap.elapsed_ns = 5 * crate::timing::NS_PER_SEC; // 20 fps reels
        let text = summary(&snap, &info(60), Path::new("/tmp/out.mp4"));
        assert!(text.contains("n'a PAS ete tenue"), "{text}");
        assert!(text.contains("20.00"), "{text}");
        assert!(text.contains("l'encodeur ne suit pas"), "{text}");
    }

    #[test]
    fn the_summary_confirms_a_target_that_was_met() {
        let stats = crate::performance::Stats::new();
        stats.frames_encoded(300);
        let mut snap = stats.snapshot();
        snap.elapsed_ns = 5 * crate::timing::NS_PER_SEC; // 60 fps
        let text = summary(&snap, &info(60), Path::new("/tmp/out.mp4"));
        assert!(text.contains("tenue"), "{text}");
        assert!(!text.contains("PAS ete tenue"), "{text}");
    }
}
