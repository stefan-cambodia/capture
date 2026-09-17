//! Abstraction de capture : ecran et son systeme.
//!
//! Les implementations specifiques a un systeme vivent dans des sous-modules
//! separes (`linux`, `windows`) et ne sont jamais melangees. Le reste du
//! pipeline ne connait que les traits [`ScreenSource`] et [`AudioSource`] et
//! les types de frame definis ici.
//!
//! # Regle du chemin de capture
//!
//! Un thread de capture ne doit **jamais** attendre l'encodeur, le muxer ou le
//! disque. Il obtient un tampon dans un pool preallouee, y depose la frame,
//! l'envoie dans une file bornee avec `try_send`, et repart immediatement. Si
//! la file est pleine, la frame est jetee et comptee : c'est la seule
//! politique qui garde la latence bornee.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::Sender;
use crossbeam_queue::ArrayQueue;

use crate::config::Config;
use crate::error::Result;
use crate::performance::Stats;

pub mod audio;
pub mod frame;
pub mod screen;
pub mod synthetic;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "windows")]
pub mod windows;

pub use frame::{AudioChunk, PixelFormat, PooledBuffer, VideoFrame};

/// Description de l'ecran capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Frequence de rafraichissement en milli-hertz (60000 = 60 Hz), si
    /// connue. Le portail xdg ne la communique pas toujours.
    pub refresh_mhz: Option<u32>,
    /// Vrai s'il s'agit de l'ecran principal.
    pub primary: bool,
}

impl fmt::Display for DisplayInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}x{}", self.name, self.width, self.height)?;
        if let Some(mhz) = self.refresh_mhz {
            write!(f, " @ {:.3} Hz", mhz as f64 / 1000.0)?;
        }
        Ok(())
    }
}

/// Format negocie de la capture audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioInfo {
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
}

impl fmt::Display for AudioInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} Hz {}",
            self.device,
            self.sample_rate,
            if self.channels == 1 { "mono" } else { "stereo" }
        )
    }
}

/// Drapeau d'arret partage entre tous les threads du pipeline.
#[derive(Debug, Default)]
pub struct StopSignal {
    stopped: AtomicBool,
}

impl StopSignal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    #[inline]
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    #[inline]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------------
// Pool de tampons
// ---------------------------------------------------------------------------

/// Pool de tampons reutilisables.
///
/// Une frame 2560x1440 BGRA pese 14,7 Mio. A 60 fps cela represente 884 Mio/s
/// d'allocations si l'on allouait a chaque frame : le pool ramene ce cout a
/// zero en regime etabli, et evite la fragmentation sur une session longue.
#[derive(Debug)]
pub struct BufferPool {
    free: ArrayQueue<Vec<u8>>,
    buffer_size: usize,
}

impl BufferPool {
    /// Prealloue `count` tampons de `buffer_size` octets.
    pub fn new(count: usize, buffer_size: usize) -> Arc<Self> {
        let count = count.max(1);
        let free = ArrayQueue::new(count);
        for _ in 0..count {
            // `vec![0; n]` passe par `calloc` : pas de memset explicite.
            let _ = free.push(vec![0u8; buffer_size]);
        }
        Arc::new(Self { free, buffer_size })
    }

    #[inline]
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// Nombre de tampons actuellement disponibles.
    #[inline]
    pub fn available(&self) -> usize {
        self.free.len()
    }

    /// Recupere un tampon. Alloue si le pool est vide, ce qui n'arrive qu'en
    /// cas de surcharge : mieux vaut une allocation qu'une frame perdue.
    pub fn acquire(self: &Arc<Self>) -> PooledBuffer {
        let buf = match self.free.pop() {
            Some(mut b) => {
                if b.len() != self.buffer_size {
                    b.resize(self.buffer_size, 0);
                }
                b
            }
            None => vec![0u8; self.buffer_size],
        };
        PooledBuffer::new(buf, Arc::clone(self))
    }

    /// Rend un tampon au pool. Appele par `PooledBuffer::drop`.
    pub(crate) fn release(&self, buf: Vec<u8>) {
        // Un tampon de mauvaise taille (resolution changee) n'est pas remis en
        // circulation : il sera libere ici.
        if buf.len() == self.buffer_size {
            let _ = self.free.push(buf);
        }
    }
}

// ---------------------------------------------------------------------------
// Sinks
// ---------------------------------------------------------------------------

/// Sortie du thread de capture video vers la file bornee.
pub struct VideoSink {
    tx: Sender<VideoFrame>,
    stats: Arc<Stats>,
    capacity: usize,
}

impl VideoSink {
    pub fn new(tx: Sender<VideoFrame>, stats: Arc<Stats>, capacity: usize) -> Self {
        Self {
            tx,
            stats,
            capacity,
        }
    }

    /// Acces aux compteurs, pour les mesures faites cote capture.
    #[inline]
    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    /// Depose une frame sans jamais bloquer.
    ///
    /// Retourne `false` si le consommateur a disparu : le thread de capture
    /// doit alors s'arreter.
    pub fn submit(&self, frame: VideoFrame) -> bool {
        self.stats.frames_captured(1);
        match self.tx.try_send(frame) {
            Ok(()) => {
                self.stats.set_video_queue(self.tx.len(), self.capacity);
                true
            }
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                // L'encodeur est en retard. On jette la frame la plus recente
                // et on le signale : jamais de blocage, jamais de silence.
                self.stats.frames_dropped_queue(1);
                self.stats.capture_overrun(1);
                self.stats.set_video_queue(self.capacity, self.capacity);
                true
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
        }
    }
}

/// Sortie du thread de capture audio.
pub struct AudioSink {
    tx: Sender<AudioChunk>,
    stats: Arc<Stats>,
    capacity: usize,
}

impl AudioSink {
    pub fn new(tx: Sender<AudioChunk>, stats: Arc<Stats>, capacity: usize) -> Self {
        Self {
            tx,
            stats,
            capacity,
        }
    }

    #[inline]
    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    pub fn submit(&self, chunk: AudioChunk) -> bool {
        let frames = chunk.frames() as u64;
        match self.tx.try_send(chunk) {
            Ok(()) => {
                self.stats.audio_chunks(1);
                self.stats.set_audio_queue(self.tx.len(), self.capacity);
                true
            }
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                // Jeter de l'audio cree un trou audible : on le compte pour
                // que la compensation de derive puisse inserer du silence.
                self.stats.audio_dropped(frames);
                self.stats.audio_overrun(1);
                self.stats.set_audio_queue(self.capacity, self.capacity);
                true
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

/// Source de capture d'ecran.
///
/// `run` est bloquant : il est execute sur un thread dedie et ne rend la main
/// qu'a l'arret ou sur erreur fatale.
pub trait ScreenSource: Send {
    /// Description de l'ecran, connue apres negociation.
    fn info(&self) -> DisplayInfo;

    /// Boucle de capture. Doit retourner des que `stop.is_stopped()`.
    ///
    /// Le sink et le signal sont pris par valeur : certains backends (PipeWire)
    /// doivent les confier a leur propre boucle d'evenements.
    fn run(&mut self, sink: VideoSink, stop: Arc<StopSignal>) -> Result<()>;
}

/// Source de capture du son systeme.
pub trait AudioSource: Send {
    fn info(&self) -> AudioInfo;

    fn run(&mut self, sink: AudioSink, stop: Arc<StopSignal>) -> Result<()>;
}

/// Ouvre la source d'ecran de la plateforme.
pub fn open_screen_source(cfg: &Config) -> Result<Box<dyn ScreenSource>> {
    screen::open(cfg)
}

/// Ouvre la source audio systeme de la plateforme.
pub fn open_audio_source(cfg: &Config) -> Result<Box<dyn AudioSource>> {
    audio::open(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_reuses_buffers_without_allocating() {
        let pool = BufferPool::new(2, 1024);
        assert_eq!(pool.available(), 2);
        let a = pool.acquire();
        let b = pool.acquire();
        assert_eq!(pool.available(), 0);
        let pa = a.as_slice().as_ptr();
        drop(a);
        drop(b);
        assert_eq!(pool.available(), 2);
        // Le tampon revient dans le pool : meme adresse, pas de realloc.
        let c = pool.acquire();
        let d = pool.acquire();
        assert!(
            c.as_slice().as_ptr() == pa || d.as_slice().as_ptr() == pa,
            "un tampon recycle devrait revenir"
        );
    }

    #[test]
    fn pool_allocates_under_pressure_rather_than_dropping() {
        let pool = BufferPool::new(1, 64);
        let _a = pool.acquire();
        let b = pool.acquire();
        assert_eq!(b.as_slice().len(), 64);
    }

    #[test]
    fn stop_signal_is_visible_across_threads() {
        let stop = StopSignal::new();
        assert!(!stop.is_stopped());
        let s = Arc::clone(&stop);
        let h = std::thread::spawn(move || {
            while !s.is_stopped() {
                std::hint::spin_loop();
            }
            true
        });
        stop.stop();
        assert!(h.join().unwrap_or(false));
    }

    #[test]
    fn video_sink_drops_instead_of_blocking_when_full() {
        let stats = Stats::shared();
        let (tx, rx) = crossbeam_channel::bounded(2);
        let sink = VideoSink::new(tx, Arc::clone(&stats), 2);
        let pool = BufferPool::new(8, 16);
        for i in 0..5 {
            let f = VideoFrame::new(pool.acquire(), 2, 2, 8, PixelFormat::Bgrx, i, i);
            assert!(sink.submit(f), "submit ne doit pas echouer");
        }
        let snap = stats.snapshot();
        assert_eq!(snap.frames_captured, 5);
        assert_eq!(snap.frames_dropped_queue, 3);
        assert!(snap.overruns.capture_overrun > 0);
        assert_eq!(rx.len(), 2);
    }

    #[test]
    fn video_sink_reports_a_dead_consumer() {
        let stats = Stats::shared();
        let (tx, rx) = crossbeam_channel::bounded(2);
        let sink = VideoSink::new(tx, stats, 2);
        drop(rx);
        let pool = BufferPool::new(2, 16);
        let f = VideoFrame::new(pool.acquire(), 2, 2, 8, PixelFormat::Bgrx, 0, 0);
        assert!(!sink.submit(f));
    }
}
