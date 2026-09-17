//! Types de frame echanges entre capture et encodage.

use std::fmt;
use std::sync::Arc;

use ffmpeg_next::format::Pixel;

use super::BufferPool;

/// Formats de pixels produits par les backends de capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 32 bits, octets B,G,R,X. Format natif de la plupart des compositeurs.
    Bgrx,
    /// 32 bits, octets B,G,R,A.
    Bgra,
    /// 32 bits, octets R,G,B,X.
    Rgbx,
    /// 32 bits, octets R,G,B,A.
    Rgba,
    /// Semi-planaire 4:2:0, entree naturelle des encodeurs materiels.
    Nv12,
}

impl PixelFormat {
    /// Format ffmpeg correspondant.
    ///
    /// Attention a l'ordre : `AV_PIX_FMT_BGRA` designe les octets B,G,R,A en
    /// memoire, ce qui correspond bien au `BGRx`/`BGRA` des compositeurs.
    pub fn to_ffmpeg(self) -> Pixel {
        match self {
            PixelFormat::Bgrx => Pixel::BGRZ,
            PixelFormat::Bgra => Pixel::BGRA,
            PixelFormat::Rgbx => Pixel::RGBZ,
            PixelFormat::Rgba => Pixel::RGBA,
            PixelFormat::Nv12 => Pixel::NV12,
        }
    }

    /// Octets par pixel du plan principal.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Nv12 => 1,
            _ => 4,
        }
    }

    /// Taille d'un tampon complet pour cette geometrie.
    pub fn buffer_size(self, stride: usize, height: usize) -> usize {
        match self {
            // Plan Y puis plan UV entrelace a demi-hauteur.
            PixelFormat::Nv12 => stride * height * 3 / 2,
            _ => stride * height,
        }
    }

    pub fn is_packed_rgb(self) -> bool {
        !matches!(self, PixelFormat::Nv12)
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PixelFormat::Bgrx => "BGRx",
            PixelFormat::Bgra => "BGRA",
            PixelFormat::Rgbx => "RGBx",
            PixelFormat::Rgba => "RGBA",
            PixelFormat::Nv12 => "NV12",
        };
        f.write_str(s)
    }
}

/// Tampon emprunte a un [`BufferPool`], rendu automatiquement au `Drop`.
pub struct PooledBuffer {
    buf: Option<Vec<u8>>,
    pool: Arc<BufferPool>,
}

impl PooledBuffer {
    pub(crate) fn new(buf: Vec<u8>, pool: Arc<BufferPool>) -> Self {
        Self {
            buf: Some(buf),
            pool,
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.buf.as_deref().unwrap_or(&[])
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.buf.as_deref_mut().unwrap_or(&mut [])
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.buf.as_ref().map_or(0, Vec::len)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        if let Some(buf) = self.buf.take() {
            self.pool.release(buf);
        }
    }
}

impl fmt::Debug for PooledBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PooledBuffer")
            .field("len", &self.len())
            .finish()
    }
}

/// Une image capturee.
#[derive(Debug)]
pub struct VideoFrame {
    buffer: PooledBuffer,
    width: u32,
    height: u32,
    /// Pas de ligne en octets. Souvent superieur a `width * bpp` : le
    /// compositeur aligne ses lignes.
    stride: u32,
    format: PixelFormat,
    /// Horodatage de presentation, horloge monotone en nanosecondes. Provient
    /// du compositeur quand il le fournit, sinon de l'instant de reception.
    pts_ns: i64,
    /// Instant de reception par notre thread, pour mesurer la latence.
    captured_ns: i64,
}

impl VideoFrame {
    pub fn new(
        buffer: PooledBuffer,
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
        pts_ns: i64,
        captured_ns: i64,
    ) -> Self {
        Self {
            buffer,
            width,
            height,
            stride,
            format,
            pts_ns,
            captured_ns,
        }
    }

    #[inline]
    pub fn data(&self) -> &[u8] {
        self.buffer.as_slice()
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    #[inline]
    pub fn stride(&self) -> u32 {
        self.stride
    }

    #[inline]
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    #[inline]
    pub fn pts_ns(&self) -> i64 {
        self.pts_ns
    }

    #[inline]
    pub fn captured_ns(&self) -> i64 {
        self.captured_ns
    }

    /// Latence entre l'instant de presentation et la reception, en us.
    #[inline]
    pub fn capture_latency_us(&self) -> u64 {
        ((self.captured_ns - self.pts_ns).max(0) / 1000) as u64
    }
}

/// Un fragment audio entrelace en `f32`.
#[derive(Debug)]
pub struct AudioChunk {
    samples: Vec<f32>,
    channels: u16,
    /// Horodatage monotone du **premier** echantillon du fragment.
    pts_ns: i64,
    captured_ns: i64,
    /// Vrai si le peripherique a signale une discontinuite avant ce fragment.
    discontinuity: bool,
}

impl AudioChunk {
    pub fn new(
        samples: Vec<f32>,
        channels: u16,
        pts_ns: i64,
        captured_ns: i64,
        discontinuity: bool,
    ) -> Self {
        Self {
            samples,
            channels,
            pts_ns,
            captured_ns,
            discontinuity,
        }
    }

    #[inline]
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    #[inline]
    pub fn into_samples(self) -> Vec<f32> {
        self.samples
    }

    #[inline]
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Nombre d'echantillons par canal.
    #[inline]
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    #[inline]
    pub fn pts_ns(&self) -> i64 {
        self.pts_ns
    }

    #[inline]
    pub fn captured_ns(&self) -> i64 {
        self.captured_ns
    }

    #[inline]
    pub fn is_discontinuity(&self) -> bool {
        self.discontinuity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_buffer_size_accounts_for_the_chroma_plane() {
        assert_eq!(PixelFormat::Nv12.buffer_size(1920, 1080), 1920 * 1080 * 3 / 2);
        assert_eq!(PixelFormat::Bgrx.buffer_size(1920 * 4, 1080), 1920 * 4 * 1080);
    }

    #[test]
    fn pixel_formats_map_to_the_matching_ffmpeg_byte_order() {
        assert_eq!(PixelFormat::Bgra.to_ffmpeg(), Pixel::BGRA);
        assert_eq!(PixelFormat::Nv12.to_ffmpeg(), Pixel::NV12);
    }

    #[test]
    fn buffer_returns_to_its_pool_on_drop() {
        let pool = BufferPool::new(1, 32);
        {
            let mut b = pool.acquire();
            b.as_mut_slice()[0] = 7;
            assert_eq!(pool.available(), 0);
        }
        assert_eq!(pool.available(), 1);
    }

    #[test]
    fn audio_chunk_frame_count_divides_by_channels() {
        let c = AudioChunk::new(vec![0.0; 960], 2, 0, 0, false);
        assert_eq!(c.frames(), 480);
        let m = AudioChunk::new(vec![0.0; 960], 1, 0, 0, false);
        assert_eq!(m.frames(), 960);
    }

    #[test]
    fn capture_latency_is_never_negative() {
        let pool = BufferPool::new(1, 16);
        // Un compositeur peut dater une frame legerement dans le futur.
        let f = VideoFrame::new(pool.acquire(), 1, 1, 4, PixelFormat::Bgrx, 1_000, 500);
        assert_eq!(f.capture_latency_us(), 0);
    }
}
