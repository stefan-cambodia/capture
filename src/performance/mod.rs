//! Statistiques temps reel du pipeline.
//!
//! Contraintes de conception :
//!
//! - Les chemins chauds (capture, encodage) ne doivent jamais bloquer pour
//!   mettre a jour une statistique. Tous les compteurs sont des entiers
//!   atomiques en ordonnancement `Relaxed` : cout ~1 ns, aucune barriere.
//! - Les histogrammes de latence sont derriere un `parking_lot::Mutex`, pris
//!   une fois par frame (~20 ns non contendu). C'est le seul verrou du chemin
//!   chaud et il n'est jamais tenu pendant une operation bloquante.
//! - Le rendu (UI, benchmark) lit un `StatsSnapshot` : une photo coherente
//!   prise a basse frequence, jamais dans la boucle d'encodage.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use hdrhistogram::Histogram;
use parking_lot::Mutex;

use crate::timing::{monotonic_ns, NS_PER_SEC};

const REL: Ordering = Ordering::Relaxed;

/// Conditions de surcharge exposees explicitement (exigence : ne jamais
/// masquer un probleme de performance).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Overruns {
    /// La file video etait pleine : le thread de capture a jete une frame.
    pub capture_overrun: u64,
    /// L'encodeur n'a pas tenu la cadence : des slots CFR ont ete sautes.
    pub encoder_overload: u64,
    /// La file audio etait pleine : des echantillons ont ete perdus.
    pub audio_overrun: u64,
    /// L'ecriture disque a bloque le muxer plus longtemps qu'une frame.
    pub disk_write_lag: u64,
}

impl Overruns {
    pub fn any(&self) -> bool {
        self.capture_overrun != 0
            || self.encoder_overload != 0
            || self.audio_overrun != 0
            || self.disk_write_lag != 0
    }
}

/// Compteurs partages. Un seul exemplaire, partage par `Arc`.
#[derive(Debug)]
pub struct Stats {
    start_ns: AtomicI64,

    // --- video ---
    frames_captured: AtomicU64,
    frames_encoded: AtomicU64,
    frames_duplicated: AtomicU64,
    frames_coalesced: AtomicU64,
    frames_late: AtomicU64,
    frames_dropped_queue: AtomicU64,
    slots_skipped: AtomicU64,

    // --- audio ---
    audio_chunks: AtomicU64,
    audio_samples: AtomicU64,
    audio_dropped: AtomicU64,
    audio_gaps_filled: AtomicU64,
    audio_compensations: AtomicU64,

    // --- muxage / disque ---
    packets_muxed: AtomicU64,
    bytes_written: AtomicU64,

    // --- jauges instantanees ---
    video_queue: AtomicU64,
    video_queue_cap: AtomicU64,
    audio_queue: AtomicU64,
    audio_queue_cap: AtomicU64,
    packet_queue: AtomicU64,
    packet_queue_cap: AtomicU64,

    // --- surcharges ---
    capture_overrun: AtomicU64,
    encoder_overload: AtomicU64,
    audio_overrun: AtomicU64,
    disk_write_lag: AtomicU64,

    /// Derive A/V accumulee, en nanosecondes (audio - video).
    av_drift_ns: AtomicI64,

    /// « Avance » de chaque flux : position media moins temps ecoule reel.
    /// C'est une mesure de la latence propre a chaque branche du pipeline.
    video_lead_ns: AtomicI64,
    audio_lead_ns: AtomicI64,
    /// Ecart d'avance observe au demarrage, retranche par la suite.
    lead_baseline_ns: AtomicI64,
    baseline_set: AtomicU64,

    hist: Mutex<Histograms>,
}

/// Histogrammes de latence.
///
/// Chaque champ est optionnel : si l'allocation echoue (parametres invalides,
/// memoire), on continue sans histogramme plutot que de paniquer dans le
/// constructeur des statistiques.
#[derive(Debug)]
struct Histograms {
    /// Latence d'encodage video (soumission -> paquet), en microsecondes.
    encode_us: Option<Histogram<u64>>,
    /// Latence de capture (timestamp de la frame -> reception), en us.
    capture_us: Option<Histogram<u64>>,
    /// Latence audio (capture -> encodage), en us.
    audio_us: Option<Histogram<u64>>,
    /// Duree d'une ecriture disque, en us.
    disk_us: Option<Histogram<u64>>,
}

/// 1 us .. 60 s, 3 chiffres significatifs : ~40 Kio par histogramme.
fn new_histogram() -> Option<Histogram<u64>> {
    Histogram::<u64>::new_with_bounds(1, 60 * 1_000_000, 3).ok()
}

impl Default for Histograms {
    fn default() -> Self {
        Self {
            encode_us: new_histogram(),
            capture_us: new_histogram(),
            audio_us: new_histogram(),
            disk_us: new_histogram(),
        }
    }
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        $(
            #[inline]
            pub fn $name(&self, n: u64) {
                self.$name.fetch_add(n, REL);
            }
        )*
    };
}

impl Stats {
    pub fn new() -> Self {
        Self {
            start_ns: AtomicI64::new(0),
            frames_captured: AtomicU64::new(0),
            frames_encoded: AtomicU64::new(0),
            frames_duplicated: AtomicU64::new(0),
            frames_coalesced: AtomicU64::new(0),
            frames_late: AtomicU64::new(0),
            frames_dropped_queue: AtomicU64::new(0),
            slots_skipped: AtomicU64::new(0),
            audio_chunks: AtomicU64::new(0),
            audio_samples: AtomicU64::new(0),
            audio_dropped: AtomicU64::new(0),
            audio_gaps_filled: AtomicU64::new(0),
            audio_compensations: AtomicU64::new(0),
            packets_muxed: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            video_queue: AtomicU64::new(0),
            video_queue_cap: AtomicU64::new(0),
            audio_queue: AtomicU64::new(0),
            audio_queue_cap: AtomicU64::new(0),
            packet_queue: AtomicU64::new(0),
            packet_queue_cap: AtomicU64::new(0),
            capture_overrun: AtomicU64::new(0),
            encoder_overload: AtomicU64::new(0),
            audio_overrun: AtomicU64::new(0),
            disk_write_lag: AtomicU64::new(0),
            av_drift_ns: AtomicI64::new(0),
            video_lead_ns: AtomicI64::new(0),
            audio_lead_ns: AtomicI64::new(0),
            lead_baseline_ns: AtomicI64::new(0),
            baseline_set: AtomicU64::new(0),
            hist: Mutex::new(Histograms::default()),
        }
    }

    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Marque le debut de l'enregistrement (horloge monotone).
    pub fn mark_start(&self, ns: i64) {
        self.start_ns.store(ns, REL);
    }

    pub fn start_ns(&self) -> i64 {
        self.start_ns.load(REL)
    }

    counters!(
        frames_captured,
        frames_encoded,
        frames_duplicated,
        frames_coalesced,
        frames_late,
        frames_dropped_queue,
        slots_skipped,
        audio_chunks,
        audio_samples,
        audio_dropped,
        audio_gaps_filled,
        audio_compensations,
        packets_muxed,
        bytes_written,
        capture_overrun,
        encoder_overload,
        audio_overrun,
        disk_write_lag,
    );

    #[inline]
    pub fn set_video_queue(&self, len: usize, cap: usize) {
        self.video_queue.store(len as u64, REL);
        self.video_queue_cap.store(cap as u64, REL);
    }

    #[inline]
    pub fn set_audio_queue(&self, len: usize, cap: usize) {
        self.audio_queue.store(len as u64, REL);
        self.audio_queue_cap.store(cap as u64, REL);
    }

    #[inline]
    pub fn set_packet_queue(&self, len: usize, cap: usize) {
        self.packet_queue.store(len as u64, REL);
        self.packet_queue_cap.store(cap as u64, REL);
    }

    /// Position media du flux video, relative a l'origine commune.
    #[inline]
    pub fn set_video_media_ns(&self, ns: i64) {
        self.video_lead_ns.store(self.lead_of(ns), REL);
        self.refresh_drift();
    }

    /// Position media du flux audio, relative a l'origine commune.
    #[inline]
    pub fn set_audio_media_ns(&self, ns: i64) {
        self.audio_lead_ns.store(self.lead_of(ns), REL);
        self.refresh_drift();
    }

    /// Avance d'un flux : sa position media moins le temps reellement ecoule.
    ///
    /// Cette valeur est negative et vaut, au signe pres, la latence de la
    /// branche consideree (attente de cadencement cote video, latence du
    /// peripherique cote audio).
    #[inline]
    fn lead_of(&self, media_ns: i64) -> i64 {
        let start = self.start_ns.load(REL);
        if start == 0 {
            return 0;
        }
        media_ns - (monotonic_ns() - start)
    }

    /// Met a jour la derive A/V.
    ///
    /// Comparer directement les positions media des deux flux donnerait la
    /// difference de **latence** entre les deux branches — plusieurs dizaines
    /// de millisecondes, constantes, et sans aucun effet sur la
    /// synchronisation du fichier produit. Ce que l'on veut mesurer, c'est ce
    /// qui *s'accumule* : on retranche donc l'ecart observe au demarrage. La
    /// derive part ainsi de zero et ne bouge que si une horloge s'eloigne
    /// reellement de l'autre.
    #[inline]
    fn refresh_drift(&self) {
        let v = self.video_lead_ns.load(REL);
        let a = self.audio_lead_ns.load(REL);
        if v == 0 || a == 0 {
            return;
        }
        let gap = a - v;
        if self.baseline_set.load(REL) == 0 {
            // On laisse passer la premiere seconde : au demarrage, les deux
            // branches n'ont pas encore atteint leur regime etabli et figer
            // la reference sur un transitoire fausserait toute la mesure.
            let start = self.start_ns.load(REL);
            if start == 0 || monotonic_ns() - start < NS_PER_SEC {
                return;
            }
            self.baseline_set.store(1, REL);
            self.lead_baseline_ns.store(gap, REL);
        }
        self.av_drift_ns
            .store(gap - self.lead_baseline_ns.load(REL), REL);
    }

    /// Latence propre a chaque branche, en nanosecondes (valeurs negatives).
    pub fn leads_ns(&self) -> (i64, i64) {
        (self.video_lead_ns.load(REL), self.audio_lead_ns.load(REL))
    }

    #[inline]
    pub fn record_encode_latency_us(&self, us: u64) {
        let mut h = self.hist.lock();
        if let Some(hist) = h.encode_us.as_mut() {
            let _ = hist.record(us.max(1));
        }
    }

    #[inline]
    pub fn record_capture_latency_us(&self, us: u64) {
        let mut h = self.hist.lock();
        if let Some(hist) = h.capture_us.as_mut() {
            let _ = hist.record(us.max(1));
        }
    }

    #[inline]
    pub fn record_audio_latency_us(&self, us: u64) {
        let mut h = self.hist.lock();
        if let Some(hist) = h.audio_us.as_mut() {
            let _ = hist.record(us.max(1));
        }
    }

    #[inline]
    pub fn record_disk_write_us(&self, us: u64) {
        let mut h = self.hist.lock();
        if let Some(hist) = h.disk_us.as_mut() {
            let _ = hist.record(us.max(1));
        }
    }

    pub fn overruns(&self) -> Overruns {
        Overruns {
            capture_overrun: self.capture_overrun.load(REL),
            encoder_overload: self.encoder_overload.load(REL),
            audio_overrun: self.audio_overrun.load(REL),
            disk_write_lag: self.disk_write_lag.load(REL),
        }
    }

    /// Photo coherente a un instant donne.
    pub fn snapshot(&self) -> StatsSnapshot {
        let now = monotonic_ns();
        let start = self.start_ns.load(REL);
        let (encode, capture, audio, disk) = {
            let h = self.hist.lock();
            (
                LatencySummary::of(h.encode_us.as_ref()),
                LatencySummary::of(h.capture_us.as_ref()),
                LatencySummary::of(h.audio_us.as_ref()),
                LatencySummary::of(h.disk_us.as_ref()),
            )
        };
        StatsSnapshot {
            at_ns: now,
            elapsed_ns: if start > 0 { now - start } else { 0 },
            frames_captured: self.frames_captured.load(REL),
            frames_encoded: self.frames_encoded.load(REL),
            frames_duplicated: self.frames_duplicated.load(REL),
            frames_coalesced: self.frames_coalesced.load(REL),
            frames_late: self.frames_late.load(REL),
            frames_dropped_queue: self.frames_dropped_queue.load(REL),
            slots_skipped: self.slots_skipped.load(REL),
            audio_chunks: self.audio_chunks.load(REL),
            audio_samples: self.audio_samples.load(REL),
            audio_dropped: self.audio_dropped.load(REL),
            audio_gaps_filled: self.audio_gaps_filled.load(REL),
            audio_compensations: self.audio_compensations.load(REL),
            packets_muxed: self.packets_muxed.load(REL),
            bytes_written: self.bytes_written.load(REL),
            video_queue: (self.video_queue.load(REL), self.video_queue_cap.load(REL)),
            audio_queue: (self.audio_queue.load(REL), self.audio_queue_cap.load(REL)),
            packet_queue: (self.packet_queue.load(REL), self.packet_queue_cap.load(REL)),
            overruns: self.overruns(),
            av_drift_ns: self.av_drift_ns.load(REL),
            encode_latency: encode,
            capture_latency: capture,
            audio_latency: audio,
            disk_latency: disk,
        }
    }
}

/// Resume d'un histogramme de latence, en microsecondes.
#[derive(Debug, Default, Clone, Copy)]
pub struct LatencySummary {
    pub count: u64,
    pub mean_us: f64,
    pub p50_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

impl LatencySummary {
    fn of(h: Option<&Histogram<u64>>) -> Self {
        match h {
            Some(h) => Self {
                count: h.len(),
                mean_us: h.mean(),
                p50_us: h.value_at_quantile(0.50),
                p99_us: h.value_at_quantile(0.99),
                max_us: h.max(),
            },
            None => Self::default(),
        }
    }
}

/// Photo des compteurs. Les debits sont calcules par difference entre deux
/// photos (voir [`Rate`]), ce qui donne une mesure exacte sans lissage cache.
#[derive(Debug, Default, Clone, Copy)]
pub struct StatsSnapshot {
    pub at_ns: i64,
    pub elapsed_ns: i64,
    pub frames_captured: u64,
    pub frames_encoded: u64,
    pub frames_duplicated: u64,
    pub frames_coalesced: u64,
    pub frames_late: u64,
    pub frames_dropped_queue: u64,
    pub slots_skipped: u64,
    pub audio_chunks: u64,
    pub audio_samples: u64,
    pub audio_dropped: u64,
    pub audio_gaps_filled: u64,
    pub audio_compensations: u64,
    pub packets_muxed: u64,
    pub bytes_written: u64,
    pub video_queue: (u64, u64),
    pub audio_queue: (u64, u64),
    pub packet_queue: (u64, u64),
    pub overruns: Overruns,
    pub av_drift_ns: i64,
    pub encode_latency: LatencySummary,
    pub capture_latency: LatencySummary,
    pub audio_latency: LatencySummary,
    pub disk_latency: LatencySummary,
}

impl StatsSnapshot {
    /// Images reellement absentes du fichier de sortie.
    ///
    /// Deux causes seulement : une file de capture saturee (l'image a ete
    /// jetee) et une surcharge de l'encodeur (des slots CFR ont ete sautes).
    ///
    /// N'en font **pas** partie :
    /// - `frames_duplicated`, des slots combles par repetition d'image, ce qui
    ///   est le comportement correct devant un ecran fixe ;
    /// - `frames_late`, des images arrivees apres l'echeance de leur slot et
    ///   re-calees sur le suivant : elles sont bien dans le fichier.
    pub fn frames_lost(&self) -> u64 {
        self.frames_dropped_queue + self.slots_skipped
    }

    pub fn duration_secs(&self) -> f64 {
        self.elapsed_ns as f64 / NS_PER_SEC as f64
    }

    pub fn av_drift_ms(&self) -> f64 {
        self.av_drift_ns as f64 / 1_000_000.0
    }
}

/// Debits calcules entre deux photos successives.
#[derive(Debug, Default, Clone, Copy)]
pub struct Rate {
    pub window_secs: f64,
    pub capture_fps: f64,
    pub encode_fps: f64,
    pub bitrate_bps: f64,
    pub disk_bps: f64,
}

impl Rate {
    pub fn between(prev: &StatsSnapshot, cur: &StatsSnapshot) -> Self {
        let dt = (cur.at_ns - prev.at_ns) as f64 / NS_PER_SEC as f64;
        if dt <= 0.0 {
            return Self::default();
        }
        let d = |a: u64, b: u64| (a.saturating_sub(b)) as f64;
        Self {
            window_secs: dt,
            capture_fps: d(cur.frames_captured, prev.frames_captured) / dt,
            encode_fps: d(cur.frames_encoded, prev.frames_encoded) / dt,
            bitrate_bps: d(cur.bytes_written, prev.bytes_written) * 8.0 / dt,
            disk_bps: d(cur.bytes_written, prev.bytes_written) / dt,
        }
    }
}

// ---------------------------------------------------------------------------
// Mesures systeme (best effort, jamais dans le chemin chaud)
// ---------------------------------------------------------------------------

/// Echantillonneur d'usage CPU du processus, base sur `/proc/self/stat`.
#[derive(Debug)]
pub struct CpuSampler {
    last_ticks: u64,
    last_ns: i64,
    ticks_per_sec: f64,
    cpus: f64,
}

impl Default for CpuSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuSampler {
    pub fn new() -> Self {
        // SAFETY: sysconf est sans effet de bord et sans pointeur.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get() as f64)
            .unwrap_or(1.0);
        Self {
            last_ticks: process_cpu_ticks().unwrap_or(0),
            last_ns: monotonic_ns(),
            ticks_per_sec: if hz > 0 { hz as f64 } else { 100.0 },
            cpus,
        }
    }

    /// Usage CPU du processus depuis le dernier appel, en pourcentage d'un
    /// coeur unique (peut depasser 100 % : pipeline multithread) et en
    /// pourcentage de la machine.
    pub fn sample(&mut self) -> Option<CpuUsage> {
        let ticks = process_cpu_ticks()?;
        let now = monotonic_ns();
        let dt = (now - self.last_ns) as f64 / NS_PER_SEC as f64;
        if dt <= 0.0 {
            return None;
        }
        let dticks = ticks.saturating_sub(self.last_ticks) as f64;
        self.last_ticks = ticks;
        self.last_ns = now;
        let cores = dticks / self.ticks_per_sec / dt;
        Some(CpuUsage {
            cores,
            percent_of_one_core: cores * 100.0,
            percent_of_machine: cores / self.cpus * 100.0,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CpuUsage {
    pub cores: f64,
    pub percent_of_one_core: f64,
    pub percent_of_machine: f64,
}

fn process_cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Le champ `comm` peut contenir des espaces : on repart d'apres ')'.
    let rest = stat.rsplit_once(')')?.1;
    let mut it = rest.split_whitespace();
    // Apres ')' : state(3) ppid(4) ... utime est le champ 14, soit l'index 11.
    let utime: u64 = it.nth(11)?.parse().ok()?;
    let stime: u64 = it.next()?.parse().ok()?;
    Some(utime + stime)
}

/// Memoire residente du processus, en octets.
pub fn resident_memory_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let rss_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf sans effet de bord.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return None;
    }
    Some(rss_pages * page as u64)
}

/// Charge GPU en pourcentage, quand le pilote l'expose.
///
/// `amdgpu` publie `gpu_busy_percent` dans sysfs. Les pilotes Intel (i915/xe)
/// n'exposent pas de compteur equivalent : on retourne `None` plutot que
/// d'inventer une valeur.
pub fn gpu_busy_percent() -> Option<f64> {
    let dir = std::fs::read_dir("/sys/class/drm").ok()?;
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let path = entry.path().join("device/gpu_busy_percent");
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(v) = s.trim().parse::<f64>() {
                return Some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate() {
        let s = Stats::new();
        s.frames_captured(3);
        s.frames_captured(2);
        assert_eq!(s.snapshot().frames_captured, 5);
    }

    #[test]
    fn lost_frames_exclude_duplicates() {
        let s = Stats::new();
        s.frames_duplicated(10);
        s.frames_dropped_queue(2);
        s.frames_late(1);
        s.slots_skipped(4);
        let snap = s.snapshot();
        // Seules la saturation de file et les slots sautes sont des pertes.
        assert_eq!(snap.frames_lost(), 6);
        assert_eq!(snap.frames_duplicated, 10);
        assert_eq!(snap.frames_late, 1);
    }

    #[test]
    fn drift_stays_zero_during_the_warm_up_period() {
        let s = Stats::new();
        s.mark_start(monotonic_ns());
        // Pendant la premiere seconde, aucune reference n'est figee : la
        // derive reste a zero plutot que d'afficher un transitoire.
        s.set_video_media_ns(0);
        s.set_audio_media_ns(-40_000_000);
        assert_eq!(s.snapshot().av_drift_ms(), 0.0);
    }

    #[test]
    fn drift_starts_at_zero_and_ignores_constant_latency() {
        let s = Stats::new();
        // Origine reculee de 2 s : la periode de chauffe est passee.
        s.mark_start(monotonic_ns() - 2 * NS_PER_SEC);
        // Les deux branches ont des latences differentes mais constantes.
        s.set_video_media_ns(0);
        s.set_audio_media_ns(-40_000_000);
        let first = s.snapshot().av_drift_ms();
        assert!(
            first.abs() < 1.0,
            "une difference de latence constante n'est pas une derive : {first}"
        );
    }

    #[test]
    fn drift_reports_a_clock_that_actually_slips() {
        let s = Stats::new();
        s.mark_start(monotonic_ns() - 2 * NS_PER_SEC);
        s.set_video_media_ns(0);
        s.set_audio_media_ns(0);
        let (v0, _) = s.leads_ns();
        // L'audio prend 30 ms d'avance sur la video, a temps reel egal.
        s.set_video_media_ns(0);
        s.set_audio_media_ns(30_000_000);
        let drift = s.snapshot().av_drift_ms();
        assert!(
            (drift - 30.0).abs() < 2.0,
            "derive attendue ~30 ms, obtenue {drift} (v0 = {v0})"
        );
    }

    #[test]
    fn rate_is_computed_between_snapshots() {
        let s = Stats::new();
        let a = s.snapshot();
        s.frames_encoded(60);
        s.bytes_written(1_000_000);
        let mut b = s.snapshot();
        // On force une fenetre de 1 s exactement pour un test deterministe.
        b.at_ns = a.at_ns + NS_PER_SEC;
        let r = Rate::between(&a, &b);
        assert!((r.encode_fps - 60.0).abs() < 1e-9);
        assert!((r.bitrate_bps - 8_000_000.0).abs() < 1e-9);
    }

    #[test]
    fn latency_percentiles_are_reported() {
        let s = Stats::new();
        for us in 1..=1000u64 {
            s.record_encode_latency_us(us);
        }
        let l = s.snapshot().encode_latency;
        assert_eq!(l.count, 1000);
        assert!(l.p99_us >= 980 && l.p99_us <= 1000, "p99 = {}", l.p99_us);
    }

    #[test]
    fn overruns_are_reported_distinctly() {
        let s = Stats::new();
        assert!(!s.overruns().any());
        s.encoder_overload(1);
        let o = s.overruns();
        assert!(o.any());
        assert_eq!(o.encoder_overload, 1);
        assert_eq!(o.capture_overrun, 0);
    }

    #[test]
    fn cpu_sampler_reports_plausible_usage() {
        let mut sampler = CpuSampler::new();
        // Un peu de travail pour que le compteur bouge.
        let mut acc = 0u64;
        let deadline = monotonic_ns() + 30_000_000;
        while monotonic_ns() < deadline {
            acc = acc.wrapping_add(1);
        }
        assert!(acc > 0);
        if let Some(u) = sampler.sample() {
            assert!(u.cores >= 0.0 && u.cores < 64.0, "cores = {}", u.cores);
        }
    }

    #[test]
    fn resident_memory_is_available_on_linux() {
        assert!(resident_memory_bytes().is_some_and(|m| m > 0));
    }
}
