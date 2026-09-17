//! Tableau de bord : la meme lecture que l'affichage terminal, en widgets.
//!
//! Il n'expose que des [`gtk::Label`] mis a jour depuis une [`StatsSnapshot`].
//! Aucune donnee n'est conservee d'un tour a l'autre sauf la photo precedente,
//! necessaire au calcul des debits — un debit se mesure entre deux instants,
//! il n'existe pas dans une photo isolee.
//!
//! Le principe du projet est repris tel quel : **ne jamais masquer un probleme
//! de performance**. Les compteurs de perte et de surcharge sont affiches en
//! permanence, et se colorent des qu'ils quittent zero.

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::performance::{Rate, StatsSnapshot};
use crate::pipeline::RecordingInfo;
use crate::ui::{format_bitrate, format_bytes, format_duration};

/// Une mesure affichee : intitule a gauche, valeur a droite.
struct Metric {
    value: gtk::Label,
    row: adw::ActionRow,
}

impl Metric {
    fn new(group: &adw::PreferencesGroup, title: &str, subtitle: &str) -> Self {
        let row = adw::ActionRow::new();
        row.set_title(title);
        if !subtitle.is_empty() {
            row.set_subtitle(subtitle);
        }
        let value = gtk::Label::new(Some("—"));
        // `.numeric` donne des chiffres de largeur fixe : sans elle, la valeur
        // tremble a chaque rafraichissement.
        value.add_css_class("numeric");
        value.add_css_class("dim-label");
        row.add_suffix(&value);
        group.add(&row);
        Self { value, row }
    }

    fn set(&self, text: &str) {
        self.value.set_text(text);
    }

    /// Affiche un compteur qui devrait rester a zero, et le signale sinon.
    fn set_fault(&self, count: u64, text: &str) {
        self.value.set_text(text);
        let bad = count > 0;
        // `error` est la classe Adwaita du rouge d'alerte ; on la retire aussi
        // bien qu'on l'ajoute, sinon un compteur remis a zero resterait rouge.
        if bad {
            self.value.remove_css_class("dim-label");
            self.value.add_css_class("error");
            self.row.add_css_class("error");
        } else {
            self.value.remove_css_class("error");
            self.row.remove_css_class("error");
            self.value.add_css_class("dim-label");
        }
    }
}

pub struct StatsView {
    pub root: gtk::Box,
    elapsed: gtk::Label,
    size: gtk::Label,
    subject: gtk::Label,
    previous: std::cell::Cell<Option<StatsSnapshot>>,

    encode_fps: Metric,
    capture_fps: Metric,
    frames_encoded: Metric,
    frames_lost: Metric,
    frames_duplicated: Metric,
    frames_coalesced: Metric,
    frames_late: Metric,

    av_drift: Metric,
    audio_gaps: Metric,
    audio_compensations: Metric,
    audio_dropped: Metric,

    video_queue: Metric,
    audio_queue: Metric,
    packet_queue: Metric,

    bitrate: Metric,
    disk: Metric,
    packets: Metric,

    encode_latency: Metric,
    capture_latency: Metric,
    audio_latency: Metric,
    disk_latency: Metric,

    capture_overrun: Metric,
    encoder_overload: Metric,
    audio_overrun: Metric,
    disk_lag: Metric,
}

impl StatsView {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 18);

        // --- bandeau : duree, taille, sujet ---
        let header = gtk::Box::new(gtk::Orientation::Vertical, 4);
        header.set_halign(gtk::Align::Center);
        let elapsed = gtk::Label::new(Some("0:00:00"));
        elapsed.add_css_class("title-1");
        elapsed.add_css_class("numeric");
        let size = gtk::Label::new(Some("0 o"));
        size.add_css_class("title-4");
        size.add_css_class("numeric");
        size.add_css_class("dim-label");
        let subject = gtk::Label::new(Some("Aucun enregistrement"));
        subject.add_css_class("caption");
        subject.add_css_class("dim-label");
        subject.set_wrap(true);
        subject.set_justify(gtk::Justification::Center);
        header.append(&elapsed);
        header.append(&size);
        header.append(&subject);
        root.append(&header);

        let cadence = adw::PreferencesGroup::new();
        cadence.set_title("Cadence");
        let encode_fps = Metric::new(&cadence, "FPS encodage", "ce qui atteint le fichier");
        let capture_fps = Metric::new(&cadence, "FPS capture", "ce que fournit le compositeur");
        let frames_encoded = Metric::new(&cadence, "Images encodees", "");
        root.append(&cadence);

        let frames = adw::PreferencesGroup::new();
        frames.set_title("Images");
        frames.set_description(Some(
            "Seules les images perdues manquent au fichier. Les images dupliquees comblent un ecran fixe, les images en retard sont recalees sur le slot suivant : elles sont bien enregistrees.",
        ));
        let frames_lost = Metric::new(&frames, "Perdues", "file saturee ou encodeur depasse");
        let frames_duplicated = Metric::new(&frames, "Dupliquees", "slots combles par repetition");
        let frames_coalesced = Metric::new(&frames, "Fusionnees", "plusieurs images pour un slot");
        let frames_late = Metric::new(&frames, "En retard", "arrivees apres leur echeance");
        root.append(&frames);

        let sync = adw::PreferencesGroup::new();
        sync.set_title("Synchronisation A/V");
        let av_drift = Metric::new(&sync, "Derive", "video moins audio");
        let audio_gaps = Metric::new(&sync, "Trous combles", "silence insere");
        let audio_compensations = Metric::new(&sync, "Corrections d'horloge", "");
        let audio_dropped = Metric::new(&sync, "Fragments perdus", "");
        root.append(&sync);

        let queues = adw::PreferencesGroup::new();
        queues.set_title("Files");
        queues.set_description(Some(
            "Une file durablement pleine designe l'etage qui suit comme goulot d'etranglement.",
        ));
        let video_queue = Metric::new(&queues, "Video", "capture -> encodeur");
        let audio_queue = Metric::new(&queues, "Audio", "capture -> encodeur");
        let packet_queue = Metric::new(&queues, "Paquets", "encodeurs -> muxer");
        root.append(&queues);

        let throughput = adw::PreferencesGroup::new();
        throughput.set_title("Debit");
        let bitrate = Metric::new(&throughput, "Flux encode", "");
        let disk = Metric::new(&throughput, "Ecriture disque", "");
        let packets = Metric::new(&throughput, "Paquets muxes", "");
        root.append(&throughput);

        let latency = adw::PreferencesGroup::new();
        latency.set_title("Latence");
        latency.set_description(Some("Moyenne et 99e centile."));
        let encode_latency = Metric::new(&latency, "Encodage", "budget = 1 / FPS");
        let capture_latency = Metric::new(&latency, "Capture", "");
        let audio_latency = Metric::new(&latency, "Audio", "");
        let disk_latency = Metric::new(&latency, "Disque", "");
        root.append(&latency);

        let faults = adw::PreferencesGroup::new();
        faults.set_title("Surcharges");
        faults.set_description(Some(
            "Ces quatre compteurs doivent rester a zero. Chacun designe un etage precis.",
        ));
        let capture_overrun = Metric::new(&faults, "File video pleine", "une image a ete jetee");
        let encoder_overload = Metric::new(&faults, "Encodeur depasse", "des slots ont ete sautes");
        let audio_overrun = Metric::new(&faults, "File audio pleine", "");
        let disk_lag = Metric::new(&faults, "Disque en retard", "ecriture plus longue qu'une image");
        root.append(&faults);

        Self {
            root,
            elapsed,
            size,
            subject,
            previous: std::cell::Cell::new(None),
            encode_fps,
            capture_fps,
            frames_encoded,
            frames_lost,
            frames_duplicated,
            frames_coalesced,
            frames_late,
            av_drift,
            audio_gaps,
            audio_compensations,
            audio_dropped,
            video_queue,
            audio_queue,
            packet_queue,
            bitrate,
            disk,
            packets,
            encode_latency,
            capture_latency,
            audio_latency,
            disk_latency,
            capture_overrun,
            encoder_overload,
            audio_overrun,
            disk_lag,
        }
    }

    /// Affiche ce qui va etre enregistre, avant la premiere image.
    pub fn set_subject(&self, info: &RecordingInfo) {
        self.subject.set_text(&format!(
            "{} — {}x{} a {} FPS — {} ({}) — {}",
            info.display,
            info.width,
            info.height,
            info.fps,
            info.encoder_name,
            if info.hardware { "materiel" } else { "logiciel" },
            info.output.display(),
        ));
    }

    pub fn set_idle(&self, text: &str) {
        self.subject.set_text(text);
    }

    /// Remet les compteurs a leur etat de repos.
    pub fn reset(&self) {
        self.previous.set(None);
        self.elapsed.set_text("0:00:00");
        self.size.set_text("0 o");
    }

    pub fn update(&self, snap: &StatsSnapshot, info: &RecordingInfo) {
        self.elapsed.set_text(&format_duration(snap.duration_secs()));
        self.size.set_text(&format_bytes(snap.bytes_written));

        // Les debits se calculent entre deux photos ; au premier tour il n'y a
        // pas de fenetre, donc pas de debit a montrer.
        let rate = self
            .previous
            .get()
            .map(|prev| Rate::between(&prev, snap))
            .unwrap_or_default();
        self.previous.set(Some(*snap));

        self.encode_fps
            .set(&format!("{:.2} / {}", rate.encode_fps, info.fps));
        self.capture_fps.set(&format!("{:.2}", rate.capture_fps));
        self.frames_encoded.set(&snap.frames_encoded.to_string());

        let lost = snap.frames_lost();
        self.frames_lost.set_fault(lost, &lost.to_string());
        self.frames_duplicated
            .set(&snap.frames_duplicated.to_string());
        self.frames_coalesced.set(&snap.frames_coalesced.to_string());
        self.frames_late.set(&snap.frames_late.to_string());

        self.av_drift.set(&format!("{:+.2} ms", snap.av_drift_ms()));
        self.audio_gaps
            .set_fault(snap.audio_gaps_filled, &snap.audio_gaps_filled.to_string());
        self.audio_compensations
            .set(&snap.audio_compensations.to_string());
        self.audio_dropped
            .set_fault(snap.audio_dropped, &snap.audio_dropped.to_string());

        let queue = |(used, cap): (u64, u64)| format!("{used} / {cap}");
        self.video_queue.set(&queue(snap.video_queue));
        self.audio_queue.set(&queue(snap.audio_queue));
        self.packet_queue.set(&queue(snap.packet_queue));

        self.bitrate.set(&format_bitrate(rate.bitrate_bps));
        self.disk
            .set(&format!("{}/s", format_bytes(rate.disk_bps as u64)));
        self.packets.set(&snap.packets_muxed.to_string());

        let latency = |l: &crate::performance::LatencySummary| {
            format!("{:.1} ms (p99 {:.1})", l.mean_us / 1000.0, l.p99_us as f64 / 1000.0)
        };
        self.encode_latency.set(&latency(&snap.encode_latency));
        self.capture_latency.set(&latency(&snap.capture_latency));
        self.audio_latency.set(&latency(&snap.audio_latency));
        self.disk_latency.set(&latency(&snap.disk_latency));

        let o = &snap.overruns;
        self.capture_overrun
            .set_fault(o.capture_overrun, &o.capture_overrun.to_string());
        self.encoder_overload
            .set_fault(o.encoder_overload, &o.encoder_overload.to_string());
        self.audio_overrun
            .set_fault(o.audio_overrun, &o.audio_overrun.to_string());
        self.disk_lag
            .set_fault(o.disk_write_lag, &o.disk_write_lag.to_string());
    }
}
