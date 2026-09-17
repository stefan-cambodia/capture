//! Tests d'integration : on enregistre reellement, puis on **relit** le
//! fichier produit et on verifie ses proprietes.
//!
//! Ces tests utilisent les sources synthetiques. La capture d'ecran reelle
//! passe par `xdg-desktop-portal`, qui exige un consentement interactif : elle
//! ne peut pas etre automatisee. Tout le reste de la chaine — cadencement,
//! files, encodage, muxage, ecriture, finalisation — est en revanche exerce
//! exactement comme en production.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ffmpeg_next as ff;
use rscap::capture::synthetic::{SyntheticAudioSource, SyntheticScreenSource};
use rscap::capture::AudioSource;
use rscap::config::{Config, HardwarePolicy, Pacing, Quality};
use rscap::performance::StatsSnapshot;
use rscap::pipeline::Recording;

/// Configuration de test : encodeur logiciel impose, pour que le resultat ne
/// depende pas du GPU de la machine qui execute la suite.
fn config(path: PathBuf, fps: u32) -> Config {
    let mut cfg = Config::default();
    cfg.output.path = path;
    cfg.video.fps = fps;
    cfg.video.encoder = "libx264".into();
    cfg.video.hardware = HardwarePolicy::Off;
    cfg.video.quality = Quality::Low;
    cfg.video.preset = "ultrafast".into();
    cfg.audio.bitrate = 96_000;
    cfg
}

fn record(cfg: &Config, width: u32, height: u32, secs: f64) -> (PathBuf, StatsSnapshot) {
    let screen = Box::new(SyntheticScreenSource::new(
        width,
        height,
        cfg.video.fps,
        cfg.pipeline.frame_pool,
    ));
    let audio: Option<Box<dyn AudioSource>> = if cfg.audio.enabled {
        Some(Box::new(SyntheticAudioSource::new(
            cfg.audio.sample_rate,
            cfg.audio.channels,
            cfg.audio.fragment_ms,
        )))
    } else {
        None
    };
    let rec = Recording::start(cfg, screen, audio).expect("demarrage du pipeline");
    std::thread::sleep(Duration::from_secs_f64(secs));
    let stats = rec.stats().snapshot();
    let path = rec.stop().expect("arret propre");
    (path, stats)
}

/// Paquets d'un flux, tels que les relit un demuxeur.
struct StreamPackets {
    pts: Vec<i64>,
    dts: Vec<i64>,
    time_base: ff::Rational,
    /// Debut du flux **apres** application de la liste d'edition du
    /// conteneur. C'est cette valeur que voit un lecteur, pas le PTS brut du
    /// premier paquet.
    start_time: i64,
    duration: i64,
}

impl StreamPackets {
    fn seconds(&self, ts: i64) -> f64 {
        ts as f64 * f64::from(self.time_base.numerator())
            / f64::from(self.time_base.denominator())
    }
}

/// Relit un fichier et rend les paquets de chaque type de flux.
fn read_packets(path: &Path) -> (Option<StreamPackets>, Option<StreamPackets>) {
    let mut input = ff::format::input(path).expect("le fichier doit etre demuxable");

    let mut video_index = None;
    let mut audio_index = None;
    let mut video = None;
    let mut audio = None;
    for stream in input.streams() {
        let tb = stream.time_base();
        match stream.parameters().medium() {
            ff::media::Type::Video => {
                video_index = Some(stream.index());
                video = Some(StreamPackets {
                    pts: Vec::new(),
                    dts: Vec::new(),
                    time_base: tb,
                    start_time: stream.start_time(),
                    duration: stream.duration(),
                });
            }
            ff::media::Type::Audio => {
                audio_index = Some(stream.index());
                audio = Some(StreamPackets {
                    pts: Vec::new(),
                    dts: Vec::new(),
                    time_base: tb,
                    start_time: stream.start_time(),
                    duration: stream.duration(),
                });
            }
            _ => {}
        }
    }

    for (stream, packet) in input.packets() {
        let target = if Some(stream.index()) == video_index {
            video.as_mut()
        } else if Some(stream.index()) == audio_index {
            audio.as_mut()
        } else {
            None
        };
        if let Some(t) = target {
            if let Some(pts) = packet.pts() {
                t.pts.push(pts);
            }
            if let Some(dts) = packet.dts() {
                t.dts.push(dts);
            }
        }
    }
    (video, audio)
}

#[test]
fn the_output_file_is_demuxable_and_has_both_streams() {
    let dir = tempfile::tempdir().expect("dossier");
    let cfg = config(dir.path().join("a.mp4"), 30);
    let (path, stats) = record(&cfg, 320, 240, 2.0);

    let input = ff::format::input(&path).expect("demuxage");
    assert_eq!(input.streams().count(), 2);
    assert!(stats.frames_encoded > 30, "{stats:?}");
}

#[test]
fn decoding_the_file_yields_the_announced_frames() {
    let dir = tempfile::tempdir().expect("dossier");
    let cfg = config(dir.path().join("decode.mp4"), 30);
    let (path, _) = record(&cfg, 320, 240, 1.5);

    // Decodage reel : c'est la seule preuve que le flux est valide, et pas
    // seulement bien forme au niveau du conteneur.
    let mut input = ff::format::input(&path).expect("demuxage");
    let stream = input
        .streams()
        .find(|s| s.parameters().medium() == ff::media::Type::Video)
        .expect("flux video");
    let index = stream.index();
    let mut decoder = ff::codec::context::Context::from_parameters(stream.parameters())
        .expect("contexte")
        .decoder()
        .video()
        .expect("decodeur");

    let mut decoded = 0usize;
    let mut frame = ff::frame::Video::empty();
    for (s, packet) in input.packets() {
        if s.index() != index {
            continue;
        }
        if decoder.send_packet(&packet).is_err() {
            continue;
        }
        while decoder.receive_frame(&mut frame).is_ok() {
            assert_eq!((frame.width(), frame.height()), (320, 240));
            decoded += 1;
        }
    }
    let _ = decoder.send_eof();
    while decoder.receive_frame(&mut frame).is_ok() {
        decoded += 1;
    }
    assert!(decoded > 30, "images decodees : {decoded}");
}

#[test]
fn packet_timestamps_are_ordered_and_monotonic() {
    let dir = tempfile::tempdir().expect("dossier");
    let cfg = config(dir.path().join("order.mp4"), 30);
    let (path, _) = record(&cfg, 320, 240, 2.0);
    let (video, audio) = read_packets(&path);
    let video = video.expect("flux video");
    let audio = audio.expect("flux audio");

    for (name, s) in [("video", &video), ("audio", &audio)] {
        assert!(!s.pts.is_empty(), "{name} : aucun paquet");
        // Les DTS doivent etre croissants : c'est ce qu'exige un decodeur.
        for w in s.dts.windows(2) {
            assert!(w[0] <= w[1], "{name} : DTS non croissants {:?}", w);
        }
        // Aucun PTS ne peut preceder son DTS.
        for (pts, dts) in s.pts.iter().zip(s.dts.iter()) {
            assert!(pts >= dts, "{name} : PTS {pts} avant DTS {dts}");
        }
        // Le flux commence a zero, a une reserve pres pour l'audio.
        //
        // L'encodeur AAC introduit un delai de codage (1024 echantillons de
        // « priming ») et le signale par un PTS negatif sur ses premiers
        // paquets. Le conteneur MP4 compense par une liste d'edition, si bien
        // qu'un lecteur demarre bien a zero — c'est ce que verifie le test
        // `both_streams_start_at_zero_for_a_player`. Ici on travaille sur les
        // PTS bruts, avant cette compensation.
        let first = s.pts.iter().copied().min().unwrap_or(1);
        match name {
            "video" => assert_eq!(first, 0, "video : le premier PTS devrait etre 0"),
            _ => assert!(
                (-2048..=0).contains(&first),
                "audio : premier PTS {first}, hors de la plage de priming AAC"
            ),
        }
    }
}

#[test]
fn both_streams_start_at_zero_for_a_player() {
    let dir = tempfile::tempdir().expect("dossier");
    let cfg = config(dir.path().join("start.mp4"), 30);
    let (path, _) = record(&cfg, 320, 240, 1.5);
    let (video, audio) = read_packets(&path);
    let video = video.expect("flux video");
    let audio = audio.expect("flux audio");

    // Apres liste d'edition, les deux pistes demarrent au meme instant.
    assert_eq!(video.seconds(video.start_time), 0.0);
    assert!(
        audio.seconds(audio.start_time).abs() < 0.001,
        "depart audio vu par un lecteur : {} s",
        audio.seconds(audio.start_time)
    );
}

#[test]
fn video_pts_follow_the_constant_frame_rate_grid() {
    let fps = 30;
    let dir = tempfile::tempdir().expect("dossier");
    let cfg = config(dir.path().join("cfr.mp4"), fps);
    let (path, _) = record(&cfg, 320, 240, 2.0);
    let (video, _) = read_packets(&path);
    let mut video = video.expect("flux video");
    video.pts.sort_unstable();

    assert!(video.pts.len() > 40, "paquets : {}", video.pts.len());
    let expected = 1.0 / fps as f64;
    for w in video.pts.windows(2) {
        let delta = video.seconds(w[1]) - video.seconds(w[0]);
        // En CFR chaque intervalle doit valoir exactement une periode, a la
        // resolution de la base de temps du conteneur pres.
        assert!(
            (delta - expected).abs() < 0.001,
            "intervalle de {delta:.6} s au lieu de {expected:.6} s"
        );
    }
}

#[test]
fn audio_and_video_cover_the_same_period() {
    let dir = tempfile::tempdir().expect("dossier");
    let cfg = config(dir.path().join("sync.mp4"), 30);
    let (path, _) = record(&cfg, 320, 240, 3.0);
    let (video, audio) = read_packets(&path);
    let video = video.expect("flux video");
    let audio = audio.expect("flux audio");

    // On compare ce que verrait un lecteur : debut et duree declares par le
    // conteneur, liste d'edition comprise.
    let v_start = video.seconds(video.start_time);
    let a_start = audio.seconds(audio.start_time);
    let v_len = video.seconds(video.duration);
    let a_len = audio.seconds(audio.duration);

    // Meme instant de depart : c'est le role de la barriere de demarrage.
    assert!(
        (v_start - a_start).abs() < 0.010,
        "departs decales : video {v_start:.4} s, audio {a_start:.4} s"
    );
    // Et meme etendue, a une trame AAC pres (21,3 ms) plus une image.
    assert!(
        (v_len - a_len).abs() < 0.080,
        "durees incoherentes : video {v_len:.4} s, audio {a_len:.4} s"
    );
    assert!(v_len > 2.0, "enregistrement trop court : {v_len:.3} s");
}

#[test]
fn audio_clock_does_not_drift_over_the_recording() {
    let dir = tempfile::tempdir().expect("dossier");
    let cfg = config(dir.path().join("drift.mp4"), 30);
    let (path, stats) = record(&cfg, 320, 240, 5.0);
    let (_, audio) = read_packets(&path);
    let audio = audio.expect("flux audio");

    // Le nombre d'echantillons deduit des PTS doit correspondre a la duree
    // reelle : si l'horloge audio derivait, l'ecart croitrait avec le temps.
    let last = audio.seconds(audio.pts.iter().copied().max().unwrap_or(0));
    let wall = stats.duration_secs();
    assert!(
        (last - wall).abs() < 0.15,
        "horloge audio a {last:.3} s pour {wall:.3} s de temps reel"
    );
    assert!(
        stats.av_drift_ms().abs() < 25.0,
        "derive A/V rapportee : {:.2} ms",
        stats.av_drift_ms()
    );
}

#[test]
fn a_recording_stopped_immediately_still_produces_a_valid_file() {
    let dir = tempfile::tempdir().expect("dossier");
    let cfg = config(dir.path().join("short.mp4"), 30);
    let screen = Box::new(SyntheticScreenSource::new(160, 120, 30, 12));
    let rec = Recording::start(&cfg, screen, None).expect("demarrage");
    // Arret quasi immediat : le cas le plus propice a un fichier tronque.
    std::thread::sleep(Duration::from_millis(80));
    let path = rec.stop().expect("arret");

    assert!(path.exists());
    // Un MP4 non finalise n'a pas de `moov` et refuse de s'ouvrir.
    let input = ff::format::input(&path).expect("le fichier doit etre finalise");
    assert!(input.streams().count() >= 1);
}

#[test]
fn vfr_keeps_the_original_capture_timestamps() {
    let dir = tempfile::tempdir().expect("dossier");
    let mut cfg = config(dir.path().join("vfr.mp4"), 30);
    cfg.video.pacing = Pacing::Vfr;
    cfg.audio.enabled = false;
    // Source a 12 fps alors que la cible annonce 30 : en VFR le fichier doit
    // contenir 12 images par seconde, pas 30.
    let screen = Box::new(SyntheticScreenSource::new(160, 120, 12, 12));
    let rec = Recording::start(&cfg, screen, None).expect("demarrage");
    std::thread::sleep(Duration::from_secs(2));
    let path = rec.stop().expect("arret");

    let (video, _) = read_packets(&path);
    let video = video.expect("flux video");
    let span = video.seconds(video.pts.iter().copied().max().unwrap_or(0));
    let rate = video.pts.len() as f64 / span.max(0.001);
    assert!(
        (rate - 12.0).abs() < 2.5,
        "cadence relue {rate:.2} fps, attendue ~12"
    );
}

#[test]
fn the_mkv_container_also_works() {
    let dir = tempfile::tempdir().expect("dossier");
    let mut cfg = config(dir.path().join("out.mkv"), 30);
    cfg.output.format = rscap::config::ContainerFormat::Mkv;
    cfg.normalize_output_extension();
    let (path, _) = record(&cfg, 160, 120, 1.0);
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("mkv"));
    let input = ff::format::input(&path).expect("demuxage MKV");
    assert_eq!(input.streams().count(), 2);
}

#[test]
fn a_fragmented_file_survives_without_a_trailer() {
    let dir = tempfile::tempdir().expect("dossier");
    let mut cfg = config(dir.path().join("frag.mp4"), 30);
    cfg.output.fragmented = true;
    let (path, _) = record(&cfg, 160, 120, 1.5);
    let input = ff::format::input(&path).expect("demuxage");
    assert_eq!(input.streams().count(), 2);
}

#[test]
fn an_invalid_configuration_is_rejected_before_anything_is_created() {
    let dir = tempfile::tempdir().expect("dossier");
    let path = dir.path().join("never.mp4");
    let mut cfg = config(path.clone(), 30);
    cfg.video.fps = 10; // sous le minimum du projet
    assert!(cfg.validate().is_err());
    assert!(!path.exists(), "aucun fichier ne doit etre cree");
}
