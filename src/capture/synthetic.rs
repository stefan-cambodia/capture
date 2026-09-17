//! Sources synthetiques : motif de test video et silence/sinus audio.
//!
//! Elles servent a deux choses :
//!
//! 1. **Tester le pipeline de bout en bout sans intervention humaine.** La
//!    capture d'ecran reelle passe par le portail xdg, qui exige un
//!    consentement interactif : impossible dans une suite de tests ou en CI.
//! 2. **Etalonner.** En mode `--benchmark` on veut mesurer l'encodeur, le
//!    muxer et le disque sans que la source d'images ne soit le facteur
//!    limitant ni une variable incontrolee.
//!
//! Le motif genere n'est pas un a-plat : un degrade mobile plus un carre en
//! mouvement produisent une charge d'encodage representative d'une capture
//! d'ecran reelle (beaucoup de zones fixes, quelques zones qui bougent).

use std::sync::Arc;

use crate::error::Result;
use crate::timing::{monotonic_ns, ns_to_duration, NS_PER_SEC};

use super::{
    AudioChunk, AudioInfo, AudioSink, AudioSource, BufferPool, DisplayInfo, PixelFormat,
    ScreenSource, StopSignal, VideoFrame, VideoSink,
};

/// Generateur d'images de test cadence a `fps`.
pub struct SyntheticScreenSource {
    info: DisplayInfo,
    fps: u32,
    pool: Arc<BufferPool>,
    /// Arret automatique apres ce nombre de frames (`None` = illimite).
    max_frames: Option<u64>,
    frame_index: u64,
}

impl SyntheticScreenSource {
    pub fn new(width: u32, height: u32, fps: u32, pool_size: usize) -> Self {
        let stride = width as usize * 4;
        let pool = BufferPool::new(pool_size, stride * height as usize);
        Self {
            info: DisplayInfo {
                name: "synthetic".into(),
                width,
                height,
                refresh_mhz: Some(fps * 1000),
                primary: true,
            },
            fps: fps.max(1),
            pool,
            max_frames: None,
            frame_index: 0,
        }
    }

    /// Limite la duree de la generation.
    pub fn with_frame_limit(mut self, frames: u64) -> Self {
        self.max_frames = Some(frames);
        self
    }

    /// Remplit un tampon BGRx avec le motif de l'image `index`.
    fn render(&self, buf: &mut [u8], index: u64) {
        let w = self.info.width as usize;
        let h = self.info.height as usize;
        let stride = w * 4;
        let phase = (index % 256) as u8;

        // Carre mobile : la zone reellement changeante d'une image a l'autre.
        let box_size = (w / 8).max(16);
        let travel = w.saturating_sub(box_size).max(1);
        let bx = (index as usize * 7) % travel;
        let by = (index as usize * 5) % h.saturating_sub(box_size).max(1);

        for y in 0..h {
            let row = &mut buf[y * stride..y * stride + stride];
            let base_g = ((y * 255) / h.max(1)) as u8;
            for x in 0..w {
                let px = &mut row[x * 4..x * 4 + 4];
                let in_box = x >= bx && x < bx + box_size && y >= by && y < by + box_size;
                if in_box {
                    px[0] = 0x20;
                    px[1] = 0xC0;
                    px[2] = 0xFF;
                } else {
                    px[0] = ((x * 255) / w.max(1)) as u8; // B
                    px[1] = base_g; // G
                    px[2] = phase; // R
                }
                px[3] = 0xFF;
            }
        }
    }
}

impl ScreenSource for SyntheticScreenSource {
    fn info(&self) -> DisplayInfo {
        self.info.clone()
    }

    fn run(&mut self, sink: VideoSink, stop: Arc<StopSignal>) -> Result<()> {
        let period_ns = NS_PER_SEC / self.fps as i64;
        let origin = monotonic_ns();
        let stride = self.info.width * 4;

        while !stop.is_stopped() {
            if let Some(max) = self.max_frames {
                if self.frame_index >= max {
                    break;
                }
            }
            // Echeance absolue : aucune derive accumulee, meme si un tour de
            // boucle deborde.
            let mut target = origin + self.frame_index as i64 * period_ns;
            let mut now = monotonic_ns();

            // Si le rendu a pris du retard, on saute a l'echeance courante au
            // lieu de produire une image deja perimee. C'est le comportement
            // d'une vraie source de capture : un compositeur en retard jette
            // l'image, il ne la livre pas avec un horodatage du passe.
            if now - target > period_ns {
                let skipped = (now - target) / period_ns;
                self.frame_index += skipped as u64;
                target = origin + self.frame_index as i64 * period_ns;
                now = monotonic_ns();
            }

            if now < target {
                // Attente courte et bornee ; on se reveille aussi pour tester
                // l'arret sans depasser 5 ms de latence.
                let wait = (target - now).min(5_000_000);
                std::thread::sleep(ns_to_duration(wait));
                continue;
            }

            let mut buf = self.pool.acquire();
            self.render(buf.as_mut_slice(), self.frame_index);
            let frame = VideoFrame::new(
                buf,
                self.info.width,
                self.info.height,
                stride,
                PixelFormat::Bgrx,
                target,
                monotonic_ns(),
            );
            if !sink.submit(frame) {
                break;
            }
            self.frame_index += 1;
        }
        Ok(())
    }
}

/// Generateur audio de test : un sinus a 440 Hz, amplitude faible.
///
/// Il produit ses fragments a la cadence reelle du temps qui passe, avec des
/// horodatages derives de l'horloge monotone, exactement comme une vraie
/// capture : le test de synchronisation a donc un sens.
pub struct SyntheticAudioSource {
    info: AudioInfo,
    fragment_frames: usize,
    phase: f64,
}

impl SyntheticAudioSource {
    pub fn new(sample_rate: u32, channels: u16, fragment_ms: u32) -> Self {
        let fragment_frames = (sample_rate as usize * fragment_ms.max(1) as usize) / 1000;
        Self {
            info: AudioInfo {
                device: "synthetic".into(),
                sample_rate,
                channels,
            },
            fragment_frames: fragment_frames.max(1),
            phase: 0.0,
        }
    }
}

impl AudioSource for SyntheticAudioSource {
    fn info(&self) -> AudioInfo {
        self.info.clone()
    }

    fn run(&mut self, sink: AudioSink, stop: Arc<StopSignal>) -> Result<()> {
        let rate = self.info.sample_rate as f64;
        let channels = self.info.channels as usize;
        let step = std::f64::consts::TAU * 440.0 / rate;
        let frag_ns = self.fragment_frames as i64 * NS_PER_SEC / self.info.sample_rate as i64;
        let origin = monotonic_ns();
        let mut produced: i64 = 0;

        while !stop.is_stopped() {
            // Le fragment n'est pret que lorsque le temps reel l'a rattrape.
            let ready_at = origin + (produced + self.fragment_frames as i64) * NS_PER_SEC
                / self.info.sample_rate as i64;
            let now = monotonic_ns();
            if now < ready_at {
                std::thread::sleep(ns_to_duration((ready_at - now).min(5_000_000)));
                continue;
            }

            let mut samples = vec![0.0f32; self.fragment_frames * channels];
            for f in 0..self.fragment_frames {
                let v = (self.phase.sin() * 0.05) as f32;
                self.phase += step;
                if self.phase > std::f64::consts::TAU {
                    self.phase -= std::f64::consts::TAU;
                }
                for c in 0..channels {
                    samples[f * channels + c] = v;
                }
            }
            let pts_ns = origin + produced * NS_PER_SEC / self.info.sample_rate as i64;
            produced += self.fragment_frames as i64;
            let _ = frag_ns;
            if !sink.submit(AudioChunk::new(
                samples,
                self.info.channels,
                pts_ns,
                monotonic_ns(),
                false,
            )) {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::performance::Stats;

    #[test]
    fn synthetic_source_produces_the_requested_geometry() {
        let stats = Stats::shared();
        let (tx, rx) = crossbeam_channel::bounded(16);
        let sink = VideoSink::new(tx, stats, 16);
        let stop = StopSignal::new();
        let mut src = SyntheticScreenSource::new(64, 32, 60, 8).with_frame_limit(3);
        src.run(sink, Arc::clone(&stop)).expect("generation");
        assert_eq!(rx.len(), 3);
        let f = rx.recv().expect("frame");
        assert_eq!((f.width(), f.height()), (64, 32));
        assert_eq!(f.stride(), 64 * 4);
        assert_eq!(f.format(), PixelFormat::Bgrx);
        assert_eq!(f.data().len(), 64 * 32 * 4);
    }

    #[test]
    fn synthetic_timestamps_are_regular_and_increasing() {
        let stats = Stats::shared();
        let (tx, rx) = crossbeam_channel::bounded(32);
        let sink = VideoSink::new(tx, stats, 32);
        let stop = StopSignal::new();
        let mut src = SyntheticScreenSource::new(32, 16, 120, 8).with_frame_limit(6);
        src.run(sink, Arc::clone(&stop)).expect("generation");
        let mut prev: Option<i64> = None;
        let period = NS_PER_SEC / 120;
        while let Ok(f) = rx.try_recv() {
            if let Some(p) = prev {
                assert_eq!(f.pts_ns() - p, period);
            }
            prev = Some(f.pts_ns());
        }
        assert!(prev.is_some());
    }

    #[test]
    fn successive_frames_actually_differ() {
        let src = SyntheticScreenSource::new(64, 64, 60, 2);
        let mut a = vec![0u8; 64 * 64 * 4];
        let mut b = vec![0u8; 64 * 64 * 4];
        src.render(&mut a, 0);
        src.render(&mut b, 1);
        assert_ne!(a, b, "le motif doit bouger pour charger l'encodeur");
    }

    #[test]
    fn synthetic_source_stops_promptly() {
        let stats = Stats::shared();
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let sink = VideoSink::new(tx, stats, 64);
        let stop = StopSignal::new();
        let s2 = Arc::clone(&stop);
        let h = std::thread::spawn(move || {
            let mut src = SyntheticScreenSource::new(32, 32, 30, 4);
            src.run(sink, s2)
        });
        std::thread::sleep(std::time::Duration::from_millis(30));
        stop.stop();
        let started = monotonic_ns();
        let _ = h.join();
        assert!(
            monotonic_ns() - started < 100_000_000,
            "l'arret doit etre quasi immediat"
        );
    }

    #[test]
    fn synthetic_audio_produces_interleaved_stereo() {
        let stats = Stats::shared();
        let (tx, rx) = crossbeam_channel::bounded(8);
        let sink = AudioSink::new(tx, stats, 8);
        let stop = StopSignal::new();
        let s2 = Arc::clone(&stop);
        let h = std::thread::spawn(move || {
            let mut src = SyntheticAudioSource::new(48_000, 2, 10);
            src.run(sink, s2)
        });
        std::thread::sleep(std::time::Duration::from_millis(60));
        stop.stop();
        let _ = h.join();
        let c = rx.recv().expect("un fragment");
        assert_eq!(c.channels(), 2);
        assert_eq!(c.frames(), 480);
        assert_eq!(c.samples().len(), 960);
    }
}
