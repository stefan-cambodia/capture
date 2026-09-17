//! Assemblage et pilotage du pipeline.
//!
//! ```text
//!  [capture video] --file bornee--> [encodage video] --+
//!        (thread)                        (thread)      |
//!                                                      +--file bornee--> [muxage] --> disque
//!  [capture audio] --file bornee--> [encodage audio] --+      (thread)
//!        (thread)                        (thread)
//! ```
//!
//! Cinq threads, quatre files bornees, aucune boucle partagee. Les regles :
//!
//! - **Un thread de capture ne bloque jamais.** File pleine = frame jetee et
//!   comptee. C'est la seule politique qui garde la latence bornee.
//! - **Les threads d'encodage peuvent bloquer sur le muxer.** C'est voulu : la
//!   contre-pression s'arrete la, sans jamais remonter jusqu'a la capture.
//! - **Le muxer est seul a ecrire.** Il finalise le fichier quoi qu'il arrive.
//! - **L'arret est pilote par un drapeau, pas par la fermeture des canaux.**
//!   Un peripherique audio bloque ne peut donc pas empecher la finalisation.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use ffmpeg_next as ff;
use ff::Rational;

use crate::capture::{
    AudioChunk, AudioInfo, AudioSink, AudioSource, DisplayInfo, ScreenSource, StopSignal,
    VideoFrame, VideoSink,
};
use crate::config::{Config, Pacing, VideoCodec};
use crate::encoder::audio::{AudioEncoder, AudioEncoderSpec, DriftAction};
use crate::encoder::hwdetect::{self, SystemInfo};
use crate::encoder::video::{VideoEncoder, VideoEncoderSpec};
use crate::encoder::PacketOut;
use crate::error::{RecorderError, Result};
use crate::muxer::{MuxCommand, Muxer};
use crate::performance::Stats;
use crate::timing::{monotonic_ns, ns_to_duration, AudioClock, CfrPacer, Emit, Ingest, NS_PER_SEC};

pub mod sync;

use sync::StartSync;

/// Delai maximal d'attente de la premiere donnee des autres flux.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Delai au-dela duquel on cesse d'attendre un thread de capture bloque.
const CAPTURE_JOIN_TIMEOUT: Duration = Duration::from_secs(3);

/// Ce que le pipeline sait de lui-meme, pour l'affichage.
#[derive(Debug, Clone)]
pub struct RecordingInfo {
    pub display: DisplayInfo,
    pub audio: Option<AudioInfo>,
    pub encoder_name: &'static str,
    pub codec: VideoCodec,
    pub hardware: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u64,
    pub output: PathBuf,
    pub system: SystemInfo,
}

/// Un thread du pipeline, joignable avec un delai maximal.
struct Worker {
    name: &'static str,
    handle: Option<JoinHandle<Result<()>>>,
    finished: Arc<AtomicBool>,
}

impl Worker {
    fn spawn<F>(name: &'static str, body: F) -> Result<Self>
    where
        F: FnOnce() -> Result<()> + Send + 'static,
    {
        let finished = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&finished);
        let handle = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let result = body();
                flag.store(true, Ordering::Release);
                result
            })
            .map_err(|e| RecorderError::PipelineAborted(format!("thread {name} : {e}")))?;
        Ok(Self {
            name,
            handle: Some(handle),
            finished,
        })
    }

    /// Attend la fin du thread, sans delai.
    fn join(&mut self) -> Result<()> {
        match self.handle.take() {
            Some(h) => h.join().unwrap_or_else(|_| {
                Err(RecorderError::ThreadPanic(self.name.to_owned()))
            }),
            None => Ok(()),
        }
    }

    /// Attend la fin du thread, puis abandonne au bout de `timeout`.
    ///
    /// Sert aux threads de capture, qui peuvent rester bloques dans un appel
    /// systeme qu'on ne peut pas interrompre (lecture audio sur un
    /// peripherique disparu). Abandonner le thread est sans danger : il ne
    /// detient que sa propre source et l'extremite d'une file.
    fn join_before(&mut self, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        while !self.finished.load(Ordering::Acquire) {
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    thread = self.name,
                    "thread de capture toujours bloque : abandon (le fichier est deja finalise)"
                );
                // On ne joint pas : le handle est simplement relache.
                self.handle.take();
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.join()
    }
}

/// Enregistrement en cours.
pub struct Recording {
    stop: Arc<StopSignal>,
    stats: Arc<Stats>,
    info: RecordingInfo,
    capture_video: Worker,
    capture_audio: Option<Worker>,
    encode_video: Worker,
    encode_audio: Option<Worker>,
    muxer: Option<JoinHandle<Result<PathBuf>>>,
    /// Conserve pour que le canal ne se ferme pas prematurement ; relache
    /// explicitement a l'arret pour que le muxer puisse terminer.
    packet_tx: Option<Sender<MuxCommand>>,
}

impl Recording {
    /// Construit et demarre tout le pipeline.
    ///
    /// Tout ce qui peut echouer (encodeur indisponible, fichier impossible a
    /// creer) echoue **ici**, avant qu'une seule image ne soit capturee.
    pub fn start(
        cfg: &Config,
        mut screen: Box<dyn ScreenSource>,
        audio: Option<Box<dyn AudioSource>>,
    ) -> Result<Self> {
        ff::init().map_err(|e| RecorderError::PipelineAborted(format!("init ffmpeg : {e}")))?;

        let display = screen.info();
        let audio_info = audio.as_ref().map(|a| a.info());
        let system = hwdetect::detect();
        let stats = Stats::shared();
        let stop = StopSignal::new();

        // --- encodeurs (avant tout le reste : ils peuvent refuser) ---
        let mut venc = VideoEncoder::open(&VideoEncoderSpec {
            width: display.width,
            height: display.height,
            cfg: cfg.video.clone(),
            global_header: true,
            system: system.clone(),
        })?;
        let mut aenc = match (&audio_info, cfg.audio.enabled) {
            (Some(info), true) => Some(AudioEncoder::open(
                &AudioEncoderSpec {
                    sample_rate: info.sample_rate,
                    channels: info.channels,
                    bitrate: cfg.audio.bitrate,
                    global_header: true,
                },
                cfg.audio.max_drift_ms,
            )?),
            _ => None,
        };

        // --- muxer et flux ---
        let mut muxer = Muxer::create(cfg, Arc::clone(&stats))?;
        let video_index = muxer.add_stream(
            venc.context(),
            Some(Rational(cfg.video.fps as i32, 1)),
        )?;
        let audio_index = match aenc.as_ref() {
            Some(a) => Some(muxer.add_stream(a.context(), None)?),
            None => None,
        };
        muxer.write_header(cfg)?;
        let video_stream_tb = muxer.stream_time_base(video_index)?;
        let audio_stream_tb = match audio_index {
            Some(i) => Some(muxer.stream_time_base(i)?),
            None => None,
        };

        let info = RecordingInfo {
            display: display.clone(),
            audio: audio_info.clone(),
            encoder_name: venc.name(),
            codec: venc.codec(),
            hardware: venc.is_hardware(),
            width: venc.width(),
            height: venc.height(),
            fps: cfg.video.fps,
            bitrate: venc.bit_rate(),
            output: muxer.path().to_path_buf(),
            system,
        };

        // --- files ---
        let (video_tx, video_rx) = bounded::<VideoFrame>(cfg.pipeline.video_queue);
        let (audio_tx, audio_rx) = bounded::<AudioChunk>(cfg.pipeline.audio_queue);
        let (packet_tx, packet_rx) = bounded::<MuxCommand>(cfg.pipeline.packet_queue);
        let packet_capacity = cfg.pipeline.packet_queue;

        let participants = if aenc.is_some() { 2 } else { 1 };
        let start_sync = Arc::new(StartSync::new(participants));

        stats.mark_start(monotonic_ns());
        stats.set_video_queue(0, cfg.pipeline.video_queue);
        stats.set_audio_queue(0, cfg.pipeline.audio_queue);
        stats.set_packet_queue(0, packet_capacity);

        // --- muxage ---
        let mux_handle = std::thread::Builder::new()
            .name("rscap-mux".into())
            .spawn(move || muxer.run(packet_rx, packet_capacity))
            .map_err(|e| RecorderError::PipelineAborted(format!("thread muxer : {e}")))?;

        // --- encodage video ---
        let encode_video = {
            let cfg = cfg.clone();
            let stats = Arc::clone(&stats);
            let stop = Arc::clone(&stop);
            let sync = Arc::clone(&start_sync);
            let tx = packet_tx.clone();
            Worker::spawn("rscap-encode-video", move || {
                video_loop(
                    &mut venc,
                    video_rx,
                    tx,
                    video_index,
                    video_stream_tb,
                    &cfg,
                    &stats,
                    &stop,
                    &sync,
                )
            })?
        };

        // --- encodage audio ---
        let encode_audio = match (aenc.take(), audio_index, audio_stream_tb) {
            (Some(mut a), Some(index), Some(tb)) => {
                let cfg = cfg.clone();
                let stats = Arc::clone(&stats);
                let stop = Arc::clone(&stop);
                let sync = Arc::clone(&start_sync);
                let tx = packet_tx.clone();
                Some(Worker::spawn("rscap-encode-audio", move || {
                    audio_loop(&mut a, audio_rx, tx, index, tb, &cfg, &stats, &stop, &sync)
                })?)
            }
            _ => {
                drop(audio_rx);
                None
            }
        };

        // --- captures ---
        let capture_video = {
            let sink = VideoSink::new(video_tx, Arc::clone(&stats), cfg.pipeline.video_queue);
            let stop = Arc::clone(&stop);
            Worker::spawn("rscap-capture-video-driver", move || screen.run(sink, stop))?
        };

        let capture_audio = match audio {
            Some(mut source) if encode_audio.is_some() => {
                let sink = AudioSink::new(audio_tx, Arc::clone(&stats), cfg.pipeline.audio_queue);
                let stop = Arc::clone(&stop);
                let sync = Arc::clone(&start_sync);
                Some(Worker::spawn("rscap-capture-audio-driver", move || {
                    let result = source.run(sink, stop);
                    if result.is_err() {
                        // Ne pas laisser la video attendre une origine
                        // commune qui n'arrivera jamais.
                        sync.withdraw();
                    }
                    result
                })?)
            }
            _ => {
                drop(audio_tx);
                start_sync.withdraw();
                None
            }
        };

        Ok(Self {
            stop,
            stats,
            info,
            capture_video,
            capture_audio,
            encode_video,
            encode_audio,
            muxer: Some(mux_handle),
            packet_tx: Some(packet_tx),
        })
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    pub fn info(&self) -> &RecordingInfo {
        &self.info
    }

    /// Demande l'arret sans attendre. Utile depuis un gestionnaire de signal.
    pub fn request_stop(&self) {
        self.stop.stop();
    }

    pub fn is_stopping(&self) -> bool {
        self.stop.is_stopped()
    }

    /// Arrete proprement et rend le chemin du fichier finalise.
    ///
    /// Ordre impose : capture, puis encodeurs (qui vident leurs files et
    /// terminent l'encodage), puis muxer (qui ecrit la fin du conteneur et
    /// synchronise le disque).
    pub fn stop(mut self) -> Result<PathBuf> {
        self.stop.stop();

        // 1. Captures. Un peripherique bloque ne doit pas retarder la suite.
        self.capture_video.join_before(CAPTURE_JOIN_TIMEOUT)?;
        let audio_capture_result = match self.capture_audio.as_mut() {
            Some(w) => w.join_before(CAPTURE_JOIN_TIMEOUT),
            None => Ok(()),
        };

        // 2. Encodeurs : ils vident leurs files puis emettent leur fin de flux.
        let video_result = self.encode_video.join();
        let audio_result = match self.encode_audio.as_mut() {
            Some(w) => w.join(),
            None => Ok(()),
        };

        // 3. Le canal peut maintenant se fermer : le muxer terminera.
        drop(self.packet_tx.take());
        let mux_result = match self.muxer.take() {
            Some(h) => h
                .join()
                .unwrap_or_else(|_| Err(RecorderError::ThreadPanic("rscap-mux".into()))),
            None => Err(RecorderError::PipelineAborted("muxer absent".into())),
        };

        // Le fichier est finalise quoi qu'il arrive ; on remonte ensuite la
        // premiere erreur rencontree, par ordre de gravite.
        let path = mux_result?;
        video_result?;
        audio_result?;
        audio_capture_result?;
        Ok(path)
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        // Filet de securite : si l'appelant oublie `stop()`, on evite au moins
        // de laisser des threads tourner indefiniment.
        self.stop.stop();
    }
}

// ---------------------------------------------------------------------------
// Boucle d'encodage video
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn video_loop(
    enc: &mut VideoEncoder,
    rx: Receiver<VideoFrame>,
    tx: Sender<MuxCommand>,
    stream_index: usize,
    stream_tb: Rational,
    cfg: &Config,
    stats: &Arc<Stats>,
    stop: &StopSignal,
    start_sync: &StartSync,
) -> Result<()> {
    let enc_tb = enc.time_base();
    let queue_cap = cfg.pipeline.video_queue;
    let mut sink = packet_sink(tx.clone(), stream_index, enc_tb, stream_tb, Arc::clone(stats));

    let result = match cfg.video.pacing {
        Pacing::Cfr => video_loop_cfr(
            enc, &rx, &mut sink, cfg, stats, stop, start_sync, queue_cap,
        ),
        Pacing::Vfr => video_loop_vfr(
            enc, &rx, &mut sink, stats, stop, start_sync, queue_cap,
        ),
    };

    // La vidange a lieu meme apres une erreur : les images deja encodees
    // doivent arriver jusqu'au fichier.
    let flush = enc.flush(&mut sink);
    drop(sink);
    let _ = tx.send(MuxCommand::VideoEof);
    result.and(flush)
}

#[allow(clippy::too_many_arguments)]
fn video_loop_cfr(
    enc: &mut VideoEncoder,
    rx: &Receiver<VideoFrame>,
    sink: &mut PacketOut<'_>,
    cfg: &Config,
    stats: &Arc<Stats>,
    stop: &StopSignal,
    start_sync: &StartSync,
    queue_cap: usize,
) -> Result<()> {
    let mut pacer = CfrPacer::new(
        cfg.video.fps,
        Duration::from_millis(cfg.video.lookahead_ms),
    );
    let period_ns = pacer.period_ns();
    /// Periode de reveil maximale : garde l'arret reactif meme sans image.
    const POLL: Duration = Duration::from_millis(20);
    let mut pending: Option<(i64, VideoFrame)> = None;
    let mut origin: Option<i64> = None;
    let mut have_content = false;

    loop {
        // On dort jusqu'a la prochaine echeance de slot — jamais une duree
        // fixe. Une iteration en retard ne decale donc pas les suivantes.
        //
        // Le pacer raisonne en temps *relatif* a l'origine commune : toute
        // lecture d'horloge doit donc etre ramenee dans ce repere avant de
        // lui etre passee.
        let timeout = match origin {
            Some(t0) => pacer
                .time_to_deadline(monotonic_ns() - t0)
                .unwrap_or(POLL)
                .min(POLL),
            None => POLL,
        };

        match rx.recv_timeout(timeout) {
            Ok(frame) => {
                stats.set_video_queue(rx.len(), queue_cap);
                let t0 = *origin.get_or_insert_with(|| {
                    start_sync.propose(frame.pts_ns(), START_TIMEOUT)
                });
                let ts = frame.pts_ns() - t0;
                if ts < 0 {
                    // Image anterieure a l'origine commune : elle appartient
                    // a la periode ou l'audio n'existait pas encore.
                    stats.frames_late(1);
                    continue;
                }
                match pacer.ingest(ts, pending.is_some()) {
                    Ingest::Late { .. } => {
                        // L'image arrive apres l'echeance de son slot. La
                        // jeter serait doublement penalisant : le slot passe
                        // a deja ete comble par duplication, et on perdrait
                        // en plus une image fraiche. On la presente donc au
                        // slot suivant — c'est exactement ce que fait une
                        // conversion de cadence — et on compte l'evenement.
                        stats.frames_late(1);
                        if pending.is_some() {
                            stats.frames_coalesced(1);
                        }
                        pending = Some((pacer.next_slot(), frame));
                    }
                    Ingest::Superseded { slot } => {
                        stats.frames_coalesced(1);
                        pending = Some((slot, frame));
                    }
                    Ingest::Accepted { slot } => pending = Some((slot, frame)),
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        // Emission des slots echus.
        let Some(t0) = origin else {
            if stop.is_stopped() && rx.is_empty() {
                break;
            }
            continue;
        };
        let mut burst = 0;
        loop {
            let now = monotonic_ns() - t0;
            let ready = pending
                .as_ref()
                .is_some_and(|(slot, _)| *slot <= pacer.next_slot());
            match pacer.emit(now, ready, have_content) {
                Emit::Wait => break,
                Emit::Skip { from, to } => {
                    let lost = (to - from).max(0) as u64;
                    stats.slots_skipped(lost);
                    stats.encoder_overload(1);
                    tracing::warn!(
                        slots = lost,
                        "encodeur sature : {lost} images sautees pour rester en temps reel"
                    );
                }
                Emit::Slot { slot, duplicated } => {
                    let source = if duplicated {
                        None
                    } else {
                        pending.take().map(|(_, f)| f)
                    };
                    if duplicated {
                        stats.frames_duplicated(1);
                    } else if let Some(f) = source.as_ref() {
                        stats.record_capture_latency_us(f.capture_latency_us());
                        have_content = true;
                    }
                    let started = monotonic_ns();
                    enc.encode(source.as_ref(), slot, sink)?;
                    stats.record_encode_latency_us(
                        ((monotonic_ns() - started) / 1000).max(0) as u64,
                    );
                    stats.frames_encoded(1);
                    stats.set_video_media_ns(slot * period_ns);
                }
            }
            burst += 1;
            if burst >= pacer.max_burst() {
                break;
            }
        }

        if stop.is_stopped() && rx.is_empty() && pending.is_none() {
            break;
        }
    }
    Ok(())
}

fn video_loop_vfr(
    enc: &mut VideoEncoder,
    rx: &Receiver<VideoFrame>,
    sink: &mut PacketOut<'_>,
    stats: &Arc<Stats>,
    stop: &StopSignal,
    start_sync: &StartSync,
    queue_cap: usize,
) -> Result<()> {
    let mut origin: Option<i64> = None;
    let mut last_pts = i64::MIN;
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(frame) => {
                stats.set_video_queue(rx.len(), queue_cap);
                let t0 = *origin.get_or_insert_with(|| {
                    start_sync.propose(frame.pts_ns(), START_TIMEOUT)
                });
                let ts = frame.pts_ns() - t0;
                if ts < 0 {
                    stats.frames_late(1);
                    continue;
                }
                // Base de temps 1/1_000_000 : le PTS est l'horodatage reel.
                let pts = ts / 1_000;
                if pts <= last_pts {
                    // Deux captures dans la meme microseconde : le conteneur
                    // exige des PTS strictement croissants.
                    stats.frames_coalesced(1);
                    continue;
                }
                last_pts = pts;
                stats.record_capture_latency_us(frame.capture_latency_us());
                let started = monotonic_ns();
                enc.encode(Some(&frame), pts, sink)?;
                stats.record_encode_latency_us(((monotonic_ns() - started) / 1000).max(0) as u64);
                stats.frames_encoded(1);
                stats.set_video_media_ns(ts);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
        if stop.is_stopped() && rx.is_empty() {
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Boucle d'encodage audio
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn audio_loop(
    enc: &mut AudioEncoder,
    rx: Receiver<AudioChunk>,
    tx: Sender<MuxCommand>,
    stream_index: usize,
    stream_tb: Rational,
    cfg: &Config,
    stats: &Arc<Stats>,
    stop: &StopSignal,
    start_sync: &StartSync,
) -> Result<()> {
    let enc_tb = enc.time_base();
    let rate = enc.rate() as i64;
    let channels = enc.channels() as usize;
    let queue_cap = cfg.pipeline.audio_queue;
    let mut sink = packet_sink(tx.clone(), stream_index, enc_tb, stream_tb, Arc::clone(stats));

    let mut clock: Option<AudioClock> = None;
    let mut origin = 0i64;

    let result = (|| -> Result<()> {
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(chunk) => {
                    stats.set_audio_queue(rx.len(), queue_cap);
                    let frames = chunk.frames();
                    if frames == 0 {
                        continue;
                    }

                    // Premier fragment : on fixe l'origine commune et on
                    // rogne ce qui la precede.
                    if clock.is_none() {
                        origin = start_sync.propose(chunk.pts_ns(), START_TIMEOUT);
                        clock = Some(AudioClock::new(enc.rate(), origin));
                    }
                    let Some(c) = clock.as_mut() else { continue };

                    let chunk_start = chunk.pts_ns();
                    let mut samples = chunk.into_samples();
                    let mut skip_frames = 0usize;
                    if chunk_start < origin {
                        skip_frames =
                            (((origin - chunk_start) * rate) / NS_PER_SEC).max(0) as usize;
                        if skip_frames >= frames {
                            continue;
                        }
                    }

                    // Derive : ou devrait-on en etre, et ou en est-on ?
                    let expected = ((chunk_start + (skip_frames as i64 * NS_PER_SEC / rate)
                        - origin)
                        * rate)
                        / NS_PER_SEC;
                    let drift = expected - c.samples();
                    match enc.plan_drift_correction(drift) {
                        DriftAction::None => {}
                        DriftAction::Compensated { delta_samples } => {
                            enc.apply_compensation(delta_samples)?;
                            stats.audio_compensations(1);
                        }
                        DriftAction::InsertSilence { frames: missing } => {
                            tracing::warn!(
                                ms = missing as i64 * 1000 / rate,
                                "trou audio comble par du silence"
                            );
                            enc.push_silence(missing, &mut sink)?;
                            c.advance(missing as i64);
                            stats.audio_gaps_filled(missing as u64);
                        }
                        DriftAction::DropSamples { frames: extra } => {
                            let extra = extra.min(frames - skip_frames);
                            tracing::warn!(
                                ms = extra as i64 * 1000 / rate,
                                "audio en avance : echantillons ecartes"
                            );
                            skip_frames += extra;
                            stats.audio_dropped(extra as u64);
                            if skip_frames >= frames {
                                continue;
                            }
                        }
                    }

                    let offset = skip_frames * channels;
                    let payload = if offset > 0 {
                        samples.drain(..offset.min(samples.len()));
                        &samples[..]
                    } else {
                        &samples[..]
                    };
                    let pushed = payload.len() / channels.max(1);
                    enc.push(payload, &mut sink)?;
                    c.advance(pushed as i64);
                    stats.set_audio_media_ns(c.media_time_ns() - origin);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
            if stop.is_stopped() && rx.is_empty() {
                break;
            }
        }
        Ok(())
    })();

    let flush = enc.flush(&mut sink);
    drop(sink);
    let _ = tx.send(MuxCommand::AudioEof);
    result.and(flush)
}

// ---------------------------------------------------------------------------
// Sortie des paquets
// ---------------------------------------------------------------------------

/// Construit le rappel qui etiquette un paquet et l'envoie au muxer.
///
/// L'envoi est **bloquant** : c'est le seul point de contre-pression du
/// pipeline. Si le disque decroche, le thread d'encodage attend ici, la file
/// de capture se remplit, et les frames en trop sont jetees et comptees — sans
/// jamais bloquer la capture elle-meme.
fn packet_sink(
    tx: Sender<MuxCommand>,
    stream_index: usize,
    enc_tb: Rational,
    stream_tb: Rational,
    stats: Arc<Stats>,
) -> PacketOut<'static> {
    Box::new(move |mut packet| {
        packet.set_stream(stream_index);
        packet.rescale_ts(enc_tb, stream_tb);
        let started = monotonic_ns();
        let result = tx
            .send(MuxCommand::Packet(packet))
            .map_err(|_| RecorderError::Mux("le muxer s'est arrete".into()));
        let waited = monotonic_ns() - started;
        if waited > 100_000_000 {
            stats.disk_write_lag(1);
            tracing::warn!(ms = waited / 1_000_000, "attente anormale du muxer");
        }
        result
    })
}

/// Duree d'attente restante avant une echeance, bornee.
#[allow(dead_code)]
fn until(deadline_ns: i64) -> Duration {
    ns_to_duration(deadline_ns - monotonic_ns())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::synthetic::{SyntheticAudioSource, SyntheticScreenSource};
    use crate::config::HardwarePolicy;

    fn test_config(path: PathBuf, fps: u32) -> Config {
        let mut cfg = Config::default();
        cfg.output.path = path;
        cfg.video.fps = fps;
        cfg.video.encoder = "libx264".into();
        cfg.video.hardware = HardwarePolicy::Off;
        cfg.video.quality = crate::config::Quality::Low;
        cfg.audio.bitrate = 96_000;
        cfg
    }

    /// Enregistre `secs` secondes avec les sources synthetiques.
    fn record(cfg: &Config, secs: f64) -> (PathBuf, crate::performance::StatsSnapshot) {
        let screen = Box::new(SyntheticScreenSource::new(320, 240, cfg.video.fps, 12));
        let audio: Option<Box<dyn AudioSource>> = if cfg.audio.enabled {
            Some(Box::new(SyntheticAudioSource::new(48_000, 2, 10)))
        } else {
            None
        };
        let rec = Recording::start(cfg, screen, audio).expect("demarrage du pipeline");
        std::thread::sleep(Duration::from_secs_f64(secs));
        let stats = rec.stats().snapshot();
        let path = rec.stop().expect("arret propre");
        (path, stats)
    }

    #[test]
    fn a_full_recording_produces_a_playable_file() {
        let dir = tempfile::tempdir().expect("dossier");
        let cfg = test_config(dir.path().join("rec.mp4"), 30);
        let (path, stats) = record(&cfg, 1.5);

        assert!(path.exists(), "fichier absent");
        let size = std::fs::metadata(&path).expect("taille").len();
        assert!(size > 5_000, "fichier trop petit : {size}");

        let input = ff::format::input(&path).expect("le fichier doit etre demuxable");
        assert_eq!(input.streams().count(), 2, "video + audio attendus");

        assert!(stats.frames_captured > 10, "{stats:?}");
        assert!(stats.frames_encoded > 10, "{stats:?}");
    }

    #[test]
    fn video_and_audio_stay_aligned() {
        let dir = tempfile::tempdir().expect("dossier");
        let cfg = test_config(dir.path().join("sync.mp4"), 30);
        let (path, _) = record(&cfg, 2.0);

        let input = ff::format::input(&path).expect("demuxage");
        let mut video_end = 0.0f64;
        let mut audio_end = 0.0f64;
        for stream in input.streams() {
            let tb = stream.time_base();
            let secs = stream.duration() as f64 * f64::from(tb.numerator())
                / f64::from(tb.denominator());
            match stream.parameters().medium() {
                ff::media::Type::Video => video_end = secs,
                ff::media::Type::Audio => audio_end = secs,
                _ => {}
            }
        }
        assert!(video_end > 0.5, "video trop courte : {video_end}");
        assert!(audio_end > 0.5, "audio trop court : {audio_end}");
        // Les deux pistes doivent couvrir la meme periode a 150 ms pres.
        assert!(
            (video_end - audio_end).abs() < 0.15,
            "desynchronisation : video {video_end:.3} s, audio {audio_end:.3} s"
        );
    }

    #[test]
    fn cfr_output_has_the_requested_frame_count() {
        let dir = tempfile::tempdir().expect("dossier");
        let cfg = test_config(dir.path().join("cfr.mp4"), 30);
        let (_, stats) = record(&cfg, 2.0);
        let expected = 30.0 * stats.duration_secs();
        let produced = stats.frames_encoded as f64;
        // A 30 fps sur 2 s on attend ~60 images ; on tolere une image de
        // bord de chaque cote.
        assert!(
            (produced - expected).abs() < 4.0,
            "images encodees {produced}, attendu ~{expected:.0}"
        );
    }

    #[test]
    fn a_static_screen_is_padded_with_duplicates_not_gaps() {
        // La source synthetique produit a 10 fps alors qu'on encode a 30 :
        // deux tiers des slots doivent etre des duplications.
        let dir = tempfile::tempdir().expect("dossier");
        let cfg = test_config(dir.path().join("dup.mp4"), 30);
        let screen = Box::new(SyntheticScreenSource::new(320, 240, 10, 12));
        let rec = Recording::start(&cfg, screen, None).expect("demarrage");
        std::thread::sleep(Duration::from_secs_f64(1.5));
        let stats = rec.stats().snapshot();
        let _ = rec.stop().expect("arret");

        assert!(stats.frames_duplicated > 10, "{stats:?}");
        assert_eq!(stats.frames_lost(), 0, "aucune perte attendue : {stats:?}");
        // Le fichier reste a 30 fps : ~3x plus d'images encodees que captees.
        assert!(
            stats.frames_encoded > stats.frames_captured * 2,
            "encodees {} vs captees {}",
            stats.frames_encoded,
            stats.frames_captured
        );
    }

    #[test]
    fn recording_without_audio_still_works() {
        let dir = tempfile::tempdir().expect("dossier");
        let mut cfg = test_config(dir.path().join("mute.mp4"), 30);
        cfg.audio.enabled = false;
        let (path, stats) = record(&cfg, 1.0);
        let input = ff::format::input(&path).expect("demuxage");
        assert_eq!(input.streams().count(), 1);
        assert!(stats.frames_encoded > 5);
    }

    #[test]
    fn vfr_output_is_also_valid() {
        let dir = tempfile::tempdir().expect("dossier");
        let mut cfg = test_config(dir.path().join("vfr.mp4"), 30);
        cfg.video.pacing = Pacing::Vfr;
        let (path, stats) = record(&cfg, 1.0);
        assert!(ff::format::input(&path).is_ok());
        // En VFR, aucune duplication : une image capturee = une image encodee.
        assert_eq!(stats.frames_duplicated, 0);
    }

    #[test]
    fn stopping_twice_is_impossible_and_the_file_is_finalized_once() {
        let dir = tempfile::tempdir().expect("dossier");
        let cfg = test_config(dir.path().join("once.mp4"), 30);
        let screen = Box::new(SyntheticScreenSource::new(160, 120, 30, 12));
        let rec = Recording::start(&cfg, screen, None).expect("demarrage");
        rec.request_stop();
        assert!(rec.is_stopping());
        // `stop` consomme l'objet : un second appel ne compile pas, ce qui
        // est la garantie recherchee.
        let path = rec.stop().expect("arret");
        assert!(path.exists());
    }

    #[test]
    fn an_impossible_output_fails_before_capturing_anything() {
        let mut cfg = Config::default();
        cfg.output.path = PathBuf::from("/proc/impossible-rscap.mp4");
        cfg.video.encoder = "libx264".into();
        cfg.video.hardware = HardwarePolicy::Off;
        let screen = Box::new(SyntheticScreenSource::new(160, 120, 30, 12));
        assert!(Recording::start(&cfg, screen, None).is_err());
    }
}
