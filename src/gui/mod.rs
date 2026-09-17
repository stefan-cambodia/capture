//! Interface graphique GTK 4 / libadwaita.
//!
//! # Ce qu'elle ajoute
//!
//! Rien au pipeline. Elle pilote exactement la meme bibliotheque que le
//! binaire en ligne de commande : meme configuration, meme validation, meme
//! [`Recording`](crate::pipeline::Recording), memes compteurs. Un reglage qui
//! n'existe pas dans `rscap.toml` n'existe pas ici non plus.
//!
//! # Repartition des threads
//!
//! Le thread principal ne fait que dessiner. Tout ce qui bloque — portail,
//! encodeurs, enregistrement, finalisation, banc d'essai — vit dans le thread
//! de [`controller`], qui communique par messages. La boucle GTK se contente
//! de vider la file d'evenements dix fois par seconde ; elle n'attend jamais
//! rien.
//!
//! # Fermeture
//!
//! Fermer la fenetre pendant un enregistrement **ne le perd pas** : la
//! fermeture est retenue, l'arret propre est demande, et la fenetre ne se
//! ferme qu'une fois le fichier finalise et synchronise sur le disque.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk4 as gtk;
use gtk::gio;
use gtk::glib;
use libadwaita as adw;

use crate::bench::BenchOptions;
use crate::config::Config;
use crate::pipeline::RecordingInfo;

mod controller;
mod settings;
mod stats;

use controller::{Command, Controller, Event, StartRequest};
use settings::SettingsForm;
use stats::StatsView;

const APP_ID: &str = "org.rscap.Rscap";

/// Periode de vidage de la file d'evenements. Le thread de commande emet a
/// 5 Hz ; relever deux fois plus souvent garantit qu'aucune photo n'attend.
const POLL: Duration = Duration::from_millis(100);

/// Resolution supposee du generateur d'images pour le banc d'essai.
const SYNTHETIC_SIZE: (u32, u32) = (2560, 1440);

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// Sources en cours d'ouverture : le portail peut demander un ecran.
    Opening,
    Recording,
    /// `stop()` en cours : les files se vident, le conteneur se termine.
    Finalizing,
}

struct App {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stack: adw::ViewStack,
    form: SettingsForm,
    stats: StatsView,
    controller: Controller,

    record_button: gtk::Button,
    state_label: gtk::Label,
    report: gtk::TextBuffer,
    probe_button: gtk::Button,
    bench_button: gtk::Button,
    bench_duration: adw::SpinRow,

    state: Cell<State>,
    /// Derniere description recue, pour alimenter le tableau de bord.
    info: RefCell<Option<RecordingInfo>>,
    /// Fermeture demandee pendant un enregistrement : elle aura lieu des que
    /// le fichier sera finalise.
    close_pending: Cell<bool>,
}

/// Point d'entree : construit l'application et rend son code de sortie.
pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        // L'interface ne prend aucun argument : ceux de la ligne de commande
        // appartiennent au binaire `rscap`.
        .flags(gio::ApplicationFlags::empty())
        .build();
    app.connect_activate(build);
    app.run_with_args::<&str>(&[])
}

fn build(application: &adw::Application) {
    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("rscap")
        .default_width(1000)
        .default_height(900)
        .build();

    let form = SettingsForm::new();
    let stats = StatsView::new();
    let stack = adw::ViewStack::new();
    let toasts = adw::ToastOverlay::new();

    // --- page d'enregistrement ---
    let record_button = gtk::Button::new();
    record_button.set_halign(gtk::Align::Center);
    record_button.add_css_class("pill");
    record_button.add_css_class("suggested-action");
    let state_label = gtk::Label::new(None);
    state_label.add_css_class("caption");
    state_label.add_css_class("dim-label");

    let record_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    record_box.set_margin_top(18);
    record_box.set_margin_bottom(6);
    record_box.append(&record_button);
    record_box.append(&state_label);

    let record_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    record_page.append(&record_box);
    record_page.append(&stats.root);
    stack.add_titled_with_icon(
        &clamped(&record_page),
        Some("enregistrement"),
        "Capture",
        "media-record-symbolic",
    );

    for page in form.pages() {
        let title = page.title().to_string();
        let name = page.name().map(|s| s.to_string()).unwrap_or_else(|| title.clone());
        let icon = page.icon_name().map(|s| s.to_string());
        stack.add_titled_with_icon(
            &page,
            Some(&name),
            &title,
            icon.as_deref().unwrap_or("preferences-system-symbolic"),
        );
    }

    // --- page diagnostic ---
    let report = gtk::TextBuffer::new(None);
    let probe_button = gtk::Button::with_label("Sonder le materiel");
    let bench_button = gtk::Button::with_label("Lancer le banc d'essai");
    let bench_duration = adw::SpinRow::with_range(1.0, 3600.0, 5.0);
    bench_duration.set_title("Duree du banc d'essai");
    bench_duration.set_subtitle("secondes");
    bench_duration.set_value(10.0);
    let diagnostics = build_diagnostics_page(&report, &probe_button, &bench_button, &bench_duration);
    stack.add_titled_with_icon(
        &diagnostics,
        Some("materiel"),
        "Materiel",
        "application-x-firmware-symbolic",
    );

    // --- barre de titre ---
    let header = adw::HeaderBar::new();
    let switcher = adw::ViewSwitcher::new();
    switcher.set_stack(Some(&stack));
    switcher.set_policy(adw::ViewSwitcherPolicy::Wide);
    header.set_title_widget(Some(&switcher));
    header.pack_end(&menu_button());

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));
    let switcher_bar = adw::ViewSwitcherBar::new();
    switcher_bar.set_stack(Some(&stack));
    toolbar.add_bottom_bar(&switcher_bar);
    toasts.set_child(Some(&toolbar));
    window.set_content(Some(&toasts));

    // Fenetre etroite : six onglets ne tiennent pas dans la barre de titre.
    // Le selecteur descend alors en bas, ou il dispose de toute la largeur.
    if let Ok(condition) = adw::BreakpointCondition::parse("max-width: 700px") {
        let breakpoint = adw::Breakpoint::new(condition);
        breakpoint.add_setter(&switcher_bar, "reveal", Some(&true.to_value()));
        // Un `None` nu est refuse : le setter veut une `Value` qui *contient*
        // l'absence de widget. La barre de titre retombe alors sur le titre.
        breakpoint.add_setter(&header, "title-widget", Some(&None::<gtk::Widget>.to_value()));
        window.add_breakpoint(breakpoint);
    }

    let app = Rc::new(App {
        window: window.clone(),
        toasts,
        stack,
        form,
        stats,
        controller: Controller::spawn(),
        record_button: record_button.clone(),
        state_label,
        report,
        probe_button: probe_button.clone(),
        bench_button: bench_button.clone(),
        bench_duration,
        state: Cell::new(State::Idle),
        info: RefCell::new(None),
        close_pending: Cell::new(false),
    });

    connect_signals(&app, &record_button, &probe_button, &bench_button);
    app.set_state(State::Idle);
    app.stats.set_idle("Aucun enregistrement");
    // La liste des moniteurs est demandee tout de suite : elle arrive par
    // message, sans bloquer l'ouverture de la fenetre.
    app.controller.send(Command::ListAudioDevices);

    let poll = app.clone();
    glib::timeout_add_local(POLL, move || {
        poll.pump();
        glib::ControlFlow::Continue
    });

    window.present();
    // Sans cela, la zone defilante s'ouvre sur la position du dernier widget
    // realise plutot qu'en haut de page. Donner le focus au bouton la ramene
    // sur lui — et rend l'action principale accessible a la touche Entree.
    app.record_button.grab_focus();
}

/// Contenu centre et de largeur bornee, comme toute fenetre Adwaita.
fn clamped(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(620);
    clamp.set_margin_top(12);
    clamp.set_margin_bottom(24);
    clamp.set_margin_start(12);
    clamp.set_margin_end(12);
    clamp.set_child(Some(child));
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    scroll.set_vexpand(true);
    scroll.set_child(Some(&clamp));
    scroll
}

fn build_diagnostics_page(
    report: &gtk::TextBuffer,
    probe: &gtk::Button,
    bench: &gtk::Button,
    duration: &adw::SpinRow,
) -> gtk::Widget {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);

    let group = adw::PreferencesGroup::new();
    group.set_title("Diagnostic");
    group.set_description(Some(
        "Le sondage ouvre reellement chaque encodeur : un nom annonce par ffmpeg ne prouve rien. Le banc d'essai mesure la chaine complete et rend un verdict.",
    ));
    group.add(duration);
    page.append(&group);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::Center);
    probe.add_css_class("pill");
    bench.add_css_class("pill");
    buttons.append(probe);
    buttons.append(bench);
    page.append(&buttons);

    let view = gtk::TextView::with_buffer(report);
    view.set_editable(false);
    view.set_monospace(true);
    view.set_cursor_visible(false);
    view.set_top_margin(8);
    view.set_left_margin(8);
    view.set_right_margin(8);
    view.set_bottom_margin(8);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_child(Some(&view));
    scroll.set_vexpand(true);
    scroll.set_min_content_height(280);
    scroll.add_css_class("card");
    page.append(&scroll);

    clamped(&page).upcast()
}

fn menu_button() -> gtk::MenuButton {
    let button = gtk::MenuButton::new();
    button.set_icon_name("open-menu-symbolic");
    button.set_tooltip_text(Some("Configuration"));
    let menu = gio::Menu::new();
    menu.append(Some("Charger une configuration…"), Some("win.load-config"));
    menu.append(Some("Enregistrer la configuration…"), Some("win.save-config"));
    menu.append(
        Some("Oublier l'autorisation d'ecran"),
        Some("win.forget-permission"),
    );
    button.set_menu_model(Some(&menu));
    button
}

fn connect_signals(
    app: &Rc<App>,
    record_button: &gtk::Button,
    probe_button: &gtk::Button,
    bench_button: &gtk::Button,
) {
    let a = app.clone();
    record_button.connect_clicked(move |_| a.toggle_recording());

    let a = app.clone();
    probe_button.connect_clicked(move |_| {
        a.set_busy(true);
        a.report.set_text("Sondage en cours…\n");
        a.controller
            .send(Command::Probe(Box::new(a.form.to_config())));
    });

    let a = app.clone();
    bench_button.connect_clicked(move |_| {
        let cfg = a.form.to_config();
        if let Err(e) = cfg.validate() {
            a.toast(&format!("Configuration refusee : {e}"));
            return;
        }
        a.set_busy(true);
        a.report.set_text("Banc d'essai en cours…\n");
        let opts = BenchOptions {
            duration: Duration::from_secs_f64(a.bench_duration.value()),
            synthetic_video: a.form.synthetic_video(),
            synthetic_audio: a.form.synthetic_audio(),
            synthetic_size: SYNTHETIC_SIZE,
        };
        a.controller.send(Command::Benchmark(Box::new(cfg), opts));
    });

    // --- actions du menu ---
    let actions = gio::SimpleActionGroup::new();

    let load = gio::SimpleAction::new("load-config", None);
    let a = app.clone();
    load.connect_activate(move |_, _| a.load_config());
    actions.add_action(&load);

    let save = gio::SimpleAction::new("save-config", None);
    let a = app.clone();
    save.connect_activate(move |_, _| a.save_config());
    actions.add_action(&save);

    let forget = gio::SimpleAction::new("forget-permission", None);
    let a = app.clone();
    forget.connect_activate(move |_, _| a.controller.send(Command::ForgetPermission));
    actions.add_action(&forget);

    app.window.insert_action_group("win", Some(&actions));

    // --- bouton « parcourir » du chemin de sortie ---
    let browse = gtk::Button::from_icon_name("document-open-symbolic");
    browse.set_valign(gtk::Align::Center);
    browse.add_css_class("flat");
    browse.set_tooltip_text(Some("Choisir le fichier de sortie"));
    let a = app.clone();
    browse.connect_clicked(move |_| a.choose_output());
    app.form.path.add_suffix(&browse);

    let a = app.clone();
    app.window.connect_close_request(move |_| a.on_close());
}

impl App {
    /// Vide la file d'evenements du thread de commande.
    fn pump(&self) {
        for event in self.controller.drain() {
            match event {
                Event::Opening => self.set_state(State::Opening),
                Event::Started(info) => {
                    self.stats.reset();
                    self.stats.set_subject(&info);
                    *self.info.borrow_mut() = Some(*info);
                    self.set_state(State::Recording);
                    // Les reglages viennent d'etre figes : la page utile est
                    // desormais le tableau de bord.
                    self.stack.set_visible_child_name("enregistrement");
                }
                Event::Tick(snap) => {
                    if let Some(info) = self.info.borrow().as_ref() {
                        self.stats.update(&snap, info);
                    }
                }
                Event::Finished { path, stats, info } => {
                    self.stats.update(&stats, &info);
                    self.stats.set_idle(&format!(
                        "{} — {} images, {} perdues",
                        path.display(),
                        stats.frames_encoded,
                        stats.frames_lost()
                    ));
                    self.set_state(State::Idle);
                    self.toast(&format!(
                        "Enregistre : {}",
                        path.file_name().unwrap_or(path.as_os_str()).to_string_lossy()
                    ));
                    self.finish_close();
                }
                Event::Failed(message) => {
                    self.set_state(State::Idle);
                    self.error_dialog(&message);
                    self.finish_close();
                }
                Event::Notice(message) => self.toast(&message),
                Event::AudioDevices(list) => {
                    if list.is_empty() {
                        self.toast("Aucun moniteur de sortie trouve.");
                    }
                    self.form.set_audio_devices(&list);
                }
                Event::Report(text) => {
                    self.report.set_text(&text);
                    self.set_busy(false);
                }
                Event::BenchReport { text, sustained } => {
                    self.report.set_text(&text);
                    self.set_busy(false);
                    self.toast(if sustained {
                        "Banc d'essai : cible tenue."
                    } else {
                        "Banc d'essai : cible NON tenue — voir le rapport."
                    });
                }
                Event::Busy(busy) => {
                    if busy && self.state.get() == State::Recording {
                        self.set_state(State::Finalizing);
                    }
                }
            }
        }
    }

    fn toggle_recording(&self) {
        match self.state.get() {
            State::Idle => {
                let cfg = self.form.to_config();
                // La validation a lieu aussi dans le thread, mais la refaire
                // ici permet de signaler l'erreur sans quitter la page.
                if let Err(e) = cfg.validate() {
                    self.toast(&format!("Configuration refusee : {e}"));
                    return;
                }
                self.controller.send(Command::Start(Box::new(StartRequest {
                    cfg,
                    synthetic_video: self.form.synthetic_video(),
                    synthetic_audio: self.form.synthetic_audio(),
                    duration: self.form.duration(),
                })));
                self.set_state(State::Opening);
            }
            State::Recording => {
                self.controller.send(Command::Stop);
                self.set_state(State::Finalizing);
            }
            // Ouverture et finalisation ne s'interrompent pas.
            State::Opening | State::Finalizing => {}
        }
    }

    fn set_state(&self, state: State) {
        self.state.set(state);
        let (label, hint, css) = match state {
            State::Idle => (
                "Demarrer l'enregistrement",
                "Pret.",
                "suggested-action",
            ),
            State::Opening => (
                "Ouverture…",
                "Choisissez l'ecran a partager dans la fenetre du systeme.",
                "suggested-action",
            ),
            State::Recording => (
                "Arreter",
                "Enregistrement en cours.",
                "destructive-action",
            ),
            State::Finalizing => (
                "Finalisation…",
                "Vidage des files, fin du conteneur, synchronisation disque.",
                "destructive-action",
            ),
        };
        self.record_button.set_label(label);
        self.record_button
            .set_sensitive(matches!(state, State::Idle | State::Recording));
        self.record_button.remove_css_class("suggested-action");
        self.record_button.remove_css_class("destructive-action");
        self.record_button.add_css_class(css);
        self.state_label.set_text(hint);

        let idle = state == State::Idle;
        self.form.set_editable(idle);
        self.probe_button.set_sensitive(idle);
        self.bench_button.set_sensitive(idle);
    }

    /// Operation longue et non interruptible hors enregistrement.
    fn set_busy(&self, busy: bool) {
        if self.state.get() != State::Idle {
            return;
        }
        self.probe_button.set_sensitive(!busy);
        self.bench_button.set_sensitive(!busy);
        self.record_button.set_sensitive(!busy);
        self.form.set_editable(!busy);
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    /// Une erreur merite mieux qu'un toast : elle peut etre longue, elle a des
    /// causes, et l'utilisateur doit pouvoir la lire a son rythme.
    fn error_dialog(&self, message: &str) {
        let dialog = adw::AlertDialog::new(Some("L'operation a echoue"), Some(message));
        dialog.add_response("ok", "Fermer");
        dialog.set_default_response(Some("ok"));
        dialog.present(Some(&self.window));
    }

    fn choose_output(&self) {
        let dialog = gtk::FileDialog::builder()
            .title("Fichier de sortie")
            .accept_label("Choisir")
            .initial_name(
                PathBuf::from(self.form.path.text().to_string())
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "recording.mp4".into()),
            )
            .build();
        let form_path = self.form.path.clone();
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if let Ok(file) = result {
                if let Some(path) = file.path() {
                    form_path.set_text(&path.to_string_lossy());
                }
            }
        });
    }

    fn load_config(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Charger une configuration")
            .filters(&toml_filters())
            .build();
        let app = self.clone();
        dialog.open(Some(&self.window), gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else { return };
            let Some(path) = file.path() else { return };
            match Config::from_toml_file(&path) {
                Ok(cfg) => {
                    app.form.apply(&cfg);
                    app.toast(&format!("Configuration chargee : {}", path.display()));
                }
                Err(e) => app.error_dialog(&e.to_string()),
            }
        });
    }

    fn save_config(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Enregistrer la configuration")
            .accept_label("Enregistrer")
            .initial_name("rscap.toml")
            .filters(&toml_filters())
            .build();
        let app = self.clone();
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else { return };
            let Some(path) = file.path() else { return };
            let cfg = app.form.to_config();
            let text = match cfg.to_toml() {
                Ok(t) => t,
                Err(e) => return app.error_dialog(&e.to_string()),
            };
            match std::fs::write(&path, text) {
                Ok(()) => app.toast(&format!("Configuration enregistree : {}", path.display())),
                Err(e) => app.error_dialog(&format!("{} : {e}", path.display())),
            }
        });
    }

    /// Fermeture : un enregistrement en cours est finalise d'abord.
    fn on_close(&self) -> glib::Propagation {
        match self.state.get() {
            State::Idle => {
                self.controller.send(Command::Shutdown);
                glib::Propagation::Proceed
            }
            State::Opening | State::Recording | State::Finalizing => {
                if !self.close_pending.get() {
                    self.close_pending.set(true);
                    self.controller.send(Command::Stop);
                    self.set_state(State::Finalizing);
                    self.toast("Finalisation du fichier avant fermeture…");
                }
                // Retenue : `finish_close` fermera la fenetre une fois le
                // fichier ecrit. Perdre un enregistrement parce qu'on a clique
                // sur la croix serait indefendable.
                glib::Propagation::Stop
            }
        }
    }

    fn finish_close(&self) {
        if self.close_pending.get() {
            self.close_pending.set(false);
            self.controller.send(Command::Shutdown);
            self.window.close();
        }
    }
}

fn toml_filters() -> gio::ListStore {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Configuration TOML"));
    filter.add_pattern("*.toml");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    filters
}
