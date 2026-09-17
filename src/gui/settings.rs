//! Formulaire de configuration : un widget par champ de [`Config`].
//!
//! # Principe
//!
//! Le formulaire n'est pas une copie de la configuration, c'est sa seule
//! representation pendant l'edition. [`SettingsForm::to_config`] la reconstruit
//! au moment ou on en a besoin — au demarrage, au diagnostic, a
//! l'enregistrement du fichier TOML — et [`SettingsForm::apply`] fait le trajet
//! inverse quand un fichier est charge. Il n'y a donc aucun etat a synchroniser
//! entre deux sources de verite.
//!
//! # Bornes
//!
//! Chaque reglage numerique porte les bornes de [`Config::validate`]. Les
//! respecter dans le widget evite a l'utilisateur de decouvrir une valeur
//! refusee au moment ou il lance un enregistrement — mais la validation reste
//! faite avant le demarrage : elle seule connait les regles croisees, comme
//! `frame_pool >= video_queue + 2`.

use std::cell::RefCell;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::config::{
    AudioConfig, Config, ContainerFormat, HardwarePolicy, OutputConfig, Pacing, PipelineConfig,
    Quality, RateControl, VideoCodec, VideoConfig,
};

/// Frequences acceptees par [`Config::validate`].
const SAMPLE_RATES: [u32; 6] = [8_000, 16_000, 22_050, 32_000, 44_100, 48_000];

/// Premiere entree de la liste des moniteurs : laisse le backend choisir.
const DEFAULT_DEVICE_LABEL: &str = "Par defaut (sortie active)";

pub struct SettingsForm {
    // --- video ---
    fps: adw::SpinRow,
    codec: adw::ComboRow,
    quality: adw::ComboRow,
    bitrate: adw::SpinRow,
    rate_control: adw::ComboRow,
    pacing: adw::ComboRow,
    hardware: adw::ComboRow,
    encoder: adw::EntryRow,
    render_node: adw::EntryRow,
    keyframe: adw::SpinRow,
    b_frames: adw::SpinRow,
    preset: adw::EntryRow,
    lookahead: adw::SpinRow,
    convert_threads: adw::SpinRow,
    // --- audio ---
    audio_enabled: adw::SwitchRow,
    audio_device: adw::ComboRow,
    /// Noms reels derriere la liste deroulante ; l'entree 0 est le defaut et
    /// ne correspond a aucun nom.
    devices: RefCell<Vec<String>>,
    sample_rate: adw::ComboRow,
    channels: adw::ComboRow,
    audio_bitrate: adw::SpinRow,
    fragment_ms: adw::SpinRow,
    drift_correction: adw::SwitchRow,
    max_drift_ms: adw::SpinRow,
    // --- sortie ---
    pub path: adw::EntryRow,
    format: adw::ComboRow,
    fragmented: adw::SwitchRow,
    writer_buffer_kb: adw::SpinRow,
    duration: adw::SpinRow,
    // --- pipeline ---
    video_queue: adw::SpinRow,
    audio_queue: adw::SpinRow,
    packet_queue: adw::SpinRow,
    frame_pool: adw::SpinRow,
    // --- sources de test ---
    synthetic_video: adw::SwitchRow,
    synthetic_audio: adw::SwitchRow,
    /// Pages construites une seule fois : `pages()` les distribue a la fenetre
    /// et `set_editable` agit directement dessus, sans avoir a remonter la
    /// hierarchie des widgets.
    pages: RefCell<Vec<adw::PreferencesPage>>,
}

fn spin(title: &str, subtitle: &str, min: f64, max: f64, step: f64) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min, max, step);
    row.set_title(title);
    if !subtitle.is_empty() {
        row.set_subtitle(subtitle);
    }
    row
}

fn combo(title: &str, subtitle: &str, options: &[&str]) -> adw::ComboRow {
    let row = adw::ComboRow::new();
    row.set_title(title);
    if !subtitle.is_empty() {
        row.set_subtitle(subtitle);
    }
    row.set_model(Some(&gtk::StringList::new(options)));
    row
}

fn switch(title: &str, subtitle: &str) -> adw::SwitchRow {
    let row = adw::SwitchRow::new();
    row.set_title(title);
    if !subtitle.is_empty() {
        row.set_subtitle(subtitle);
    }
    row
}

fn entry(title: &str) -> adw::EntryRow {
    let row = adw::EntryRow::new();
    row.set_title(title);
    row
}

fn group(title: &str, description: &str) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::new();
    g.set_title(title);
    if !description.is_empty() {
        g.set_description(Some(description));
    }
    g
}

fn page(title: &str, icon: &str, name: &str) -> adw::PreferencesPage {
    let p = adw::PreferencesPage::new();
    p.set_title(title);
    p.set_icon_name(Some(icon));
    p.set_name(Some(name));
    p
}

impl SettingsForm {
    pub fn new() -> Self {
        let form = Self {
            fps: spin("Images par seconde", "30 minimum : objectif du projet", 30.0, 480.0, 1.0),
            codec: combo("Codec", "", &["H.264", "HEVC", "AV1"]),
            quality: combo(
                "Qualite",
                "Pilote le debit quand celui-ci est automatique",
                &["Basse", "Moyenne", "Haute", "Tres haute"],
            ),
            bitrate: spin(
                "Debit impose",
                "0 = calcule depuis la qualite et la resolution",
                0.0,
                500_000_000.0,
                500_000.0,
            ),
            rate_control: combo(
                "Controle de debit",
                "VBR borne le debit, CBR le fige, CQ vise une qualite constante",
                &["VBR", "CBR", "CQ"],
            ),
            pacing: combo(
                "Cadencement",
                "CFR : grille reguliere, compatible partout. VFR : horodatage de capture",
                &["CFR", "VFR"],
            ),
            hardware: combo(
                "Encodage materiel",
                "Auto retombe sur le logiciel ; Force echoue si le materiel manque",
                &["Auto", "Force", "Off"],
            ),
            encoder: entry("Encodeur impose"),
            render_node: entry("Noeud de rendu"),
            keyframe: spin("Intervalle d'images cles", "secondes", 0.1, 60.0, 0.1),
            b_frames: spin("Images B", "0 pour la latence la plus basse", 0.0, 16.0, 1.0),
            preset: entry("Preset de l'encodeur"),
            lookahead: spin(
                "Anticipation",
                "ms — tolerance de jitter du cadenceur",
                0.0,
                1000.0,
                1.0,
            ),
            convert_threads: spin("Fils de conversion", "0 = automatique", 0.0, 64.0, 1.0),

            audio_enabled: switch("Capturer le son systeme", "Sortie, pas microphone"),
            audio_device: combo("Moniteur", "", &[DEFAULT_DEVICE_LABEL]),
            devices: RefCell::new(Vec::new()),
            sample_rate: combo(
                "Frequence",
                "",
                &["8 kHz", "16 kHz", "22.05 kHz", "32 kHz", "44.1 kHz", "48 kHz"],
            ),
            channels: combo("Canaux", "", &["Mono", "Stereo"]),
            audio_bitrate: spin("Debit audio", "bits par seconde", 32_000.0, 512_000.0, 8_000.0),
            fragment_ms: spin("Fragment", "ms lus par tour de capture", 1.0, 100.0, 1.0),
            drift_correction: switch(
                "Correction de derive",
                "Asservit l'horloge audio sur celle de la video",
            ),
            max_drift_ms: spin("Derive maximale", "ms avant correction brutale", 10.0, 5000.0, 10.0),

            path: entry("Fichier de sortie"),
            format: combo("Conteneur", "", &["MP4", "MKV"]),
            fragmented: switch(
                "MP4 fragmente",
                "Lisible meme apres un arret brutal, moins compatible en montage",
            ),
            writer_buffer_kb: spin("Tampon d'ecriture", "Kio", 64.0, 262_144.0, 64.0),
            duration: spin("Arret automatique", "secondes — 0 = illimite", 0.0, 86_400.0, 10.0),

            video_queue: spin("File video", "images en vol capture -> encodeur", 2.0, 240.0, 1.0),
            audio_queue: spin("File audio", "fragments en vol", 2.0, 4096.0, 1.0),
            packet_queue: spin("File de paquets", "paquets en vol encodeurs -> muxer", 8.0, 8192.0, 1.0),
            frame_pool: spin(
                "Pool de tampons",
                "doit valoir au moins file video + 2",
                4.0,
                512.0,
                1.0,
            ),

            synthetic_video: switch(
                "Images synthetiques",
                "Generateur au lieu de l'ecran : aucune demande de portail",
            ),
            synthetic_audio: switch("Son synthetique", "Generateur au lieu du moniteur systeme"),
            pages: RefCell::new(Vec::new()),
        };
        form.keyframe.set_digits(1);
        form.apply(&Config::default());
        *form.pages.borrow_mut() = form.build_pages();
        form
    }

    /// Les quatre pages de reglages, dans l'ordre d'apparition.
    pub fn pages(&self) -> Vec<adw::PreferencesPage> {
        self.pages.borrow().clone()
    }

    fn build_pages(&self) -> Vec<adw::PreferencesPage> {
        let video = page("Video", "video-display-symbolic", "video");
        let g = group("Image", "");
        g.add(&self.fps);
        g.add(&self.codec);
        g.add(&self.quality);
        g.add(&self.bitrate);
        g.add(&self.rate_control);
        g.add(&self.pacing);
        video.add(&g);
        let g = group(
            "Encodeur",
            "Laisser vide pour la selection automatique, qui essaie le materiel puis retombe sur le logiciel.",
        );
        g.add(&self.hardware);
        g.add(&self.encoder);
        g.add(&self.render_node);
        video.add(&g);
        let g = group("Avance", "");
        g.add(&self.keyframe);
        g.add(&self.b_frames);
        g.add(&self.preset);
        g.add(&self.lookahead);
        g.add(&self.convert_threads);
        video.add(&g);

        let audio = page("Audio", "audio-volume-high-symbolic", "audio");
        let g = group("Source", "");
        g.add(&self.audio_enabled);
        g.add(&self.audio_device);
        audio.add(&g);
        let g = group("Format", "");
        g.add(&self.sample_rate);
        g.add(&self.channels);
        g.add(&self.audio_bitrate);
        audio.add(&g);
        let g = group(
            "Synchronisation",
            "L'horloge d'une carte son n'a aucune raison d'etre exactement a 48 kHz : sans correction, l'ecart s'accumule.",
        );
        g.add(&self.fragment_ms);
        g.add(&self.drift_correction);
        g.add(&self.max_drift_ms);
        audio.add(&g);

        let output = page("Sortie", "document-save-symbolic", "sortie");
        let g = group("Fichier", "");
        g.add(&self.path);
        g.add(&self.format);
        g.add(&self.fragmented);
        output.add(&g);
        let g = group("Duree", "");
        g.add(&self.duration);
        output.add(&g);
        let g = group("Disque", "");
        g.add(&self.writer_buffer_kb);
        output.add(&g);

        let advanced = page("Pipeline", "preferences-system-symbolic", "pipeline");
        let g = group(
            "Files",
            "Elles decouplent les etages. Plus longues, elles absorbent mieux les rafales ; plus courtes, elles bornent la latence et la memoire.",
        );
        g.add(&self.video_queue);
        g.add(&self.audio_queue);
        g.add(&self.packet_queue);
        g.add(&self.frame_pool);
        advanced.add(&g);
        let g = group(
            "Sources de test",
            "Remplacent le materiel par un generateur deterministe : utile pour mesurer l'encodage et le disque sans le compositeur, et pour essayer l'application sans partager d'ecran.",
        );
        g.add(&self.synthetic_video);
        g.add(&self.synthetic_audio);
        advanced.add(&g);

        vec![video, audio, output, advanced]
    }

    /// Reconstruit la configuration depuis les widgets.
    pub fn to_config(&self) -> Config {
        Config {
            video: VideoConfig {
                fps: self.fps.value() as u32,
                codec: match self.codec.selected() {
                    0 => VideoCodec::H264,
                    1 => VideoCodec::Hevc,
                    _ => VideoCodec::Av1,
                },
                quality: match self.quality.selected() {
                    0 => Quality::Low,
                    1 => Quality::Medium,
                    2 => Quality::High,
                    _ => Quality::VeryHigh,
                },
                bitrate: self.bitrate.value() as u64,
                rate_control: match self.rate_control.selected() {
                    0 => RateControl::Vbr,
                    1 => RateControl::Cbr,
                    _ => RateControl::Cq,
                },
                keyframe_interval_secs: self.keyframe.value(),
                b_frames: self.b_frames.value() as u32,
                preset: self.preset.text().to_string(),
                hardware: match self.hardware.selected() {
                    0 => HardwarePolicy::Auto,
                    1 => HardwarePolicy::Force,
                    _ => HardwarePolicy::Off,
                },
                encoder: self.encoder.text().trim().to_string(),
                render_node: self.render_node.text().trim().to_string(),
                pacing: match self.pacing.selected() {
                    0 => Pacing::Cfr,
                    _ => Pacing::Vfr,
                },
                lookahead_ms: self.lookahead.value() as u64,
                convert_threads: self.convert_threads.value() as u32,
            },
            audio: AudioConfig {
                enabled: self.audio_enabled.is_active(),
                sample_rate: SAMPLE_RATES
                    .get(self.sample_rate.selected() as usize)
                    .copied()
                    .unwrap_or(48_000),
                channels: if self.channels.selected() == 0 { 1 } else { 2 },
                bitrate: self.audio_bitrate.value() as u64,
                device: self.selected_device(),
                fragment_ms: self.fragment_ms.value() as u32,
                drift_correction: self.drift_correction.is_active(),
                max_drift_ms: self.max_drift_ms.value() as u32,
            },
            output: OutputConfig {
                path: self.path.text().trim().into(),
                format: if self.format.selected() == 0 {
                    ContainerFormat::Mp4
                } else {
                    ContainerFormat::Mkv
                },
                fragmented: self.fragmented.is_active(),
                writer_buffer_kb: self.writer_buffer_kb.value() as usize,
            },
            pipeline: PipelineConfig {
                video_queue: self.video_queue.value() as usize,
                audio_queue: self.audio_queue.value() as usize,
                packet_queue: self.packet_queue.value() as usize,
                frame_pool: self.frame_pool.value() as usize,
            },
        }
    }

    /// Recharge tous les widgets depuis une configuration.
    pub fn apply(&self, cfg: &Config) {
        self.fps.set_value(cfg.video.fps as f64);
        self.codec.set_selected(match cfg.video.codec {
            VideoCodec::H264 => 0,
            VideoCodec::Hevc => 1,
            VideoCodec::Av1 => 2,
        });
        self.quality.set_selected(match cfg.video.quality {
            Quality::Low => 0,
            Quality::Medium => 1,
            Quality::High => 2,
            Quality::VeryHigh => 3,
        });
        self.bitrate.set_value(cfg.video.bitrate as f64);
        self.rate_control.set_selected(match cfg.video.rate_control {
            RateControl::Vbr => 0,
            RateControl::Cbr => 1,
            RateControl::Cq => 2,
        });
        self.pacing.set_selected(match cfg.video.pacing {
            Pacing::Cfr => 0,
            Pacing::Vfr => 1,
        });
        self.hardware.set_selected(match cfg.video.hardware {
            HardwarePolicy::Auto => 0,
            HardwarePolicy::Force => 1,
            HardwarePolicy::Off => 2,
        });
        self.encoder.set_text(&cfg.video.encoder);
        self.render_node.set_text(&cfg.video.render_node);
        self.keyframe.set_value(cfg.video.keyframe_interval_secs);
        self.b_frames.set_value(cfg.video.b_frames as f64);
        self.preset.set_text(&cfg.video.preset);
        self.lookahead.set_value(cfg.video.lookahead_ms as f64);
        self.convert_threads.set_value(cfg.video.convert_threads as f64);

        self.audio_enabled.set_active(cfg.audio.enabled);
        self.select_device(&cfg.audio.device);
        self.sample_rate.set_selected(
            SAMPLE_RATES
                .iter()
                .position(|r| *r == cfg.audio.sample_rate)
                .unwrap_or(SAMPLE_RATES.len() - 1) as u32,
        );
        self.channels.set_selected(u32::from(cfg.audio.channels > 1));
        self.audio_bitrate.set_value(cfg.audio.bitrate as f64);
        self.fragment_ms.set_value(cfg.audio.fragment_ms as f64);
        self.drift_correction.set_active(cfg.audio.drift_correction);
        self.max_drift_ms.set_value(cfg.audio.max_drift_ms as f64);

        self.path.set_text(&cfg.output.path.to_string_lossy());
        self.format
            .set_selected(u32::from(cfg.output.format == ContainerFormat::Mkv));
        self.fragmented.set_active(cfg.output.fragmented);
        self.writer_buffer_kb
            .set_value(cfg.output.writer_buffer_kb as f64);

        self.video_queue.set_value(cfg.pipeline.video_queue as f64);
        self.audio_queue.set_value(cfg.pipeline.audio_queue as f64);
        self.packet_queue.set_value(cfg.pipeline.packet_queue as f64);
        self.frame_pool.set_value(cfg.pipeline.frame_pool as f64);
    }

    /// Remplit la liste des moniteurs, en conservant la selection courante.
    pub fn set_audio_devices(&self, devices: &[String]) {
        let current = self.selected_device();
        let mut labels = vec![DEFAULT_DEVICE_LABEL.to_string()];
        labels.extend(devices.iter().cloned());
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.audio_device.set_model(Some(&gtk::StringList::new(&refs)));
        *self.devices.borrow_mut() = devices.to_vec();
        self.select_device(&current);
    }

    fn selected_device(&self) -> String {
        let index = self.audio_device.selected();
        if index == 0 {
            return String::new();
        }
        self.devices
            .borrow()
            .get(index as usize - 1)
            .cloned()
            .unwrap_or_default()
    }

    /// Selectionne un moniteur par son nom, ou le defaut s'il a disparu.
    fn select_device(&self, name: &str) {
        if name.is_empty() {
            self.audio_device.set_selected(0);
            return;
        }
        let index = self
            .devices
            .borrow()
            .iter()
            .position(|d| d == name)
            .map(|i| i as u32 + 1)
            .unwrap_or(0);
        self.audio_device.set_selected(index);
    }

    pub fn duration(&self) -> Option<f64> {
        let secs = self.duration.value();
        (secs > 0.0).then_some(secs)
    }

    pub fn synthetic_video(&self) -> bool {
        self.synthetic_video.is_active()
    }

    pub fn synthetic_audio(&self) -> bool {
        self.synthetic_audio.is_active()
    }

    /// Fige ou libere l'edition. Pendant un enregistrement, la configuration
    /// est celle qui a ete envoyee au pipeline : la modifier n'aurait aucun
    /// effet et laisserait croire le contraire.
    pub fn set_editable(&self, editable: bool) {
        for page in self.pages.borrow().iter() {
            page.set_sensitive(editable);
        }
    }
}
