//! Capture d'ecran via PipeWire, alimentee par le portail xdg.
//!
//! # Pourquoi un thread proprietaire
//!
//! Les objets PipeWire (`MainLoop`, `Stream`) sont a comptage de reference
//! non atomique : ils ne traversent pas les threads. On les confine donc dans
//! un unique thread, cree des l'ouverture de la source, qui :
//!
//! 1. negocie la session avec le portail,
//! 2. connecte le flux et attend la negociation du format,
//! 3. publie la geometrie reelle vers l'appelant,
//! 4. boucle sur les evenements PipeWire jusqu'a l'arret.
//!
//! Il faut connaitre la geometrie **avant** de construire l'encodeur : c'est
//! pour cela que l'etape 3 existe, plutot que de se fier a la taille annoncee
//! par le portail (qui est une taille logique, faussee par la mise a l'echelle
//! fractionnaire).
//!
//! # Chemin d'une image
//!
//! ```text
//! compositeur --(memoire partagee PipeWire)--> callback `process`
//!     -> copie unique vers un tampon du pool
//!     -> try_send dans la file bornee    (jamais bloquant)
//!     -> le tampon PipeWire est rendu immediatement
//! ```
//!
//! Le callback `process` s'execute sur le thread temps reel de PipeWire : il
//! ne contient qu'un `memcpy` et un envoi non bloquant. Aucune allocation,
//! aucun verrou, aucune E/S.

use std::cell::RefCell;
use std::mem::size_of;
use std::rc::Rc;
use std::sync::{Arc, Once};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use pipewire as pw;
use pw::spa;
use spa::buffer::meta::MetaHeader;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::{Pod, Property, Value};
use spa::utils::{Fraction, Rectangle};

use crate::capture::{
    BufferPool, DisplayInfo, PixelFormat, ScreenSource, StopSignal, VideoFrame, VideoSink,
};
use crate::config::Config;
use crate::error::{RecorderError, Result};
use crate::timing::monotonic_ns;

use super::portal::PortalSession;

/// Delai maximal d'attente de la boite de dialogue du portail.
const PORTAL_TIMEOUT: Duration = Duration::from_secs(120);

static PW_INIT: Once = Once::new();

fn init_pipewire() {
    PW_INIT.call_once(|| {
        pw::init();
    });
}

/// Messages envoyes au thread PipeWire.
enum Control {
    /// Fournit la destination des images : la capture commence.
    Start(VideoSink),
    /// Demande l'arret de la boucle d'evenements.
    Quit,
}

/// Geometrie reellement negociee.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    width: u32,
    height: u32,
    format: PixelFormat,
}

/// Etat partage entre les callbacks de la boucle PipeWire.
struct Shared {
    cfg_pool_size: usize,
    sink: Option<VideoSink>,
    pool: Option<Arc<BufferPool>>,
    geometry: Option<Geometry>,
    /// Pas de ligne retenu lors de la creation du pool.
    stride: u32,
    announced: bool,
    ready: Sender<Result<DisplayInfo>>,
    /// Evite d'inonder les logs depuis un callback temps reel.
    warned_missing_header: bool,
    warned_short_buffer: bool,
}

impl Shared {
    /// Cree le pool a la premiere image, quand le pas de ligne reel est connu.
    fn ensure_pool(&mut self, stride: u32, height: u32, format: PixelFormat) -> Option<&Arc<BufferPool>> {
        let needed = format.buffer_size(stride as usize, height as usize);
        let stale = self
            .pool
            .as_ref()
            .is_none_or(|p| p.buffer_size() != needed);
        if stale {
            self.stride = stride;
            self.pool = Some(BufferPool::new(self.cfg_pool_size, needed));
        }
        self.pool.as_ref()
    }
}

/// Source d'ecran Wayland/X11 via portail + PipeWire.
pub struct PipeWireScreenSource {
    info: DisplayInfo,
    control: pw::channel::Sender<Control>,
    done: Receiver<Result<()>>,
    worker: Option<JoinHandle<()>>,
}

impl PipeWireScreenSource {
    /// Negocie la session et attend que le format soit fixe.
    pub fn open(cfg: &Config) -> Result<Self> {
        init_pipewire();

        let (control_tx, control_rx) = pw::channel::channel::<Control>();
        let (ready_tx, ready_rx) = bounded::<Result<DisplayInfo>>(1);
        let (done_tx, done_rx) = bounded::<Result<()>>(1);

        let fps = cfg.video.fps;
        let pool_size = cfg.pipeline.frame_pool;

        let worker = std::thread::Builder::new()
            .name("rscap-capture-video".into())
            .spawn(move || {
                let ready_for_error = ready_tx.clone();
                let result = worker_main(fps, pool_size, ready_tx, control_rx);
                if let Err(e) = &result {
                    // Si l'erreur survient avant la negociation, l'appelant
                    // attend toujours : on le debloque.
                    let _ = ready_for_error.try_send(Err(clone_error(e)));
                }
                let _ = done_tx.send(result);
            })
            .map_err(|e| RecorderError::ScreenCapture(format!("thread de capture : {e}")))?;

        let info = match ready_rx.recv_timeout(PORTAL_TIMEOUT) {
            Ok(Ok(info)) => info,
            Ok(Err(e)) => {
                let _ = worker.join();
                return Err(e);
            }
            Err(_) => {
                return Err(RecorderError::ScreenCapture(
                    "aucune reponse du portail : autorisation refusee ou boite de dialogue ignoree"
                        .into(),
                ))
            }
        };

        Ok(Self {
            info,
            control: control_tx,
            done: done_rx,
            worker: Some(worker),
        })
    }
}

impl ScreenSource for PipeWireScreenSource {
    fn info(&self) -> DisplayInfo {
        self.info.clone()
    }

    fn run(&mut self, sink: VideoSink, stop: Arc<StopSignal>) -> Result<()> {
        if self.control.send(Control::Start(sink)).is_err() {
            return Err(RecorderError::ScreenCapture(
                "le thread PipeWire s'est arrete avant le demarrage".into(),
            ));
        }

        // On surveille l'arret sans interroger PipeWire : le cout est nul et
        // la latence d'arret est bornee a 50 ms.
        let outcome = loop {
            match self.done.recv_timeout(Duration::from_millis(50)) {
                Ok(res) => break res,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if stop.is_stopped() {
                        let _ = self.control.send(Control::Quit);
                        break self
                            .done
                            .recv_timeout(Duration::from_secs(5))
                            .unwrap_or(Ok(()));
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break Ok(()),
            }
        };

        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        outcome
    }
}

impl Drop for PipeWireScreenSource {
    fn drop(&mut self) {
        let _ = self.control.send(Control::Quit);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// Les erreurs ne sont pas `Clone` : on en fabrique une equivalente pour
/// pouvoir la signaler a la fois a l'ouverture et a la terminaison.
fn clone_error(e: &RecorderError) -> RecorderError {
    RecorderError::ScreenCapture(e.to_string())
}

fn worker_main(
    fps: u32,
    pool_size: usize,
    ready: Sender<Result<DisplayInfo>>,
    control_rx: pw::channel::Receiver<Control>,
) -> Result<()> {
    // 1. Autorisation et canal PipeWire.
    let portal = PortalSession::open(true)?;
    let node_id = portal.node_id;
    // `connect_fd_rc` prend possession du descripteur ; on le duplique pour
    // que `PortalSession` garde le sien et puisse fermer proprement.
    let fd = portal
        .fd
        .try_clone()
        .map_err(|e| RecorderError::ScreenCapture(format!("duplication du descripteur : {e}")))?;

    // 2. Boucle et flux.
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| RecorderError::ScreenCapture(format!("boucle PipeWire : {e}")))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| RecorderError::ScreenCapture(format!("contexte PipeWire : {e}")))?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| RecorderError::ScreenCapture(format!("connexion a PipeWire : {e}")))?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "rscap-screen",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::NODE_NAME => "rscap",
        },
    )
    .map_err(|e| RecorderError::ScreenCapture(format!("creation du flux : {e}")))?;

    let shared = Rc::new(RefCell::new(Shared {
        cfg_pool_size: pool_size,
        sink: None,
        pool: None,
        geometry: None,
        stride: 0,
        announced: false,
        ready,
        warned_missing_header: false,
        warned_short_buffer: false,
    }));

    let listener = {
        let shared_param = Rc::clone(&shared);
        let shared_process = Rc::clone(&shared);
        stream
            .add_local_listener_with_user_data(VideoInfoRaw::default())
            .state_changed(|_, _, old, new| {
                tracing::debug!(?old, ?new, "etat du flux PipeWire");
            })
            .param_changed(move |stream, info, id, param| {
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else { return };
                let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
                else {
                    return;
                };
                if media_type != spa::param::format::MediaType::Video
                    || media_subtype != spa::param::format::MediaSubtype::Raw
                {
                    return;
                }
                if info.parse(param).is_err() {
                    return;
                }

                let size = info.size();
                let Some(format) = map_video_format(info.format()) else {
                    tracing::error!(format = ?info.format(), "format de pixels non gere");
                    return;
                };

                // Demande explicite de la metadonnee d'en-tete : sans elle,
                // aucun horodatage du compositeur, donc des PTS de moins bonne
                // qualite (voir `process`).
                let meta = spa::pod::object!(
                    spa::utils::SpaTypes::ObjectParamMeta,
                    spa::param::ParamType::Meta,
                    Property::new(
                        spa::sys::SPA_PARAM_META_type,
                        Value::Id(spa::utils::Id(spa::sys::SPA_META_Header)),
                    ),
                    Property::new(
                        spa::sys::SPA_PARAM_META_size,
                        Value::Int(size_of::<spa::sys::spa_meta_header>() as i32),
                    ),
                );
                if let Some(bytes) = serialize_object(&meta) {
                    if let Some(pod) = Pod::from_bytes(&bytes) {
                        let _ = stream.update_params(&mut [pod]);
                    }
                }

                let framerate = info.max_framerate();
                let refresh_mhz = if framerate.denom > 0 && framerate.num > 0 {
                    Some(framerate.num.saturating_mul(1000) / framerate.denom)
                } else {
                    None
                };

                let mut s = shared_param.borrow_mut();
                s.geometry = Some(Geometry {
                    width: size.width,
                    height: size.height,
                    format,
                });
                if !s.announced {
                    s.announced = true;
                    let _ = s.ready.try_send(Ok(DisplayInfo {
                        name: "portal:monitor".into(),
                        width: size.width,
                        height: size.height,
                        refresh_mhz,
                        primary: true,
                    }));
                }
                tracing::info!(
                    width = size.width,
                    height = size.height,
                    format = %format,
                    "format de capture negocie"
                );
            })
            .process(move |stream, _| {
                on_process(stream, &shared_process);
            })
            .register()
            .map_err(|e| RecorderError::ScreenCapture(format!("ecouteur du flux : {e}")))?
    };

    // 3. Formats acceptes.
    let format_pod = build_enum_format(fps);
    let Some(pod) = Pod::from_bytes(&format_pod) else {
        return Err(RecorderError::ScreenCapture(
            "construction du descripteur de format impossible".into(),
        ));
    };
    let mut params = [pod];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| RecorderError::ScreenCapture(format!("connexion du flux : {e}")))?;

    // 4. Canal de controle attache a la boucle.
    let mainloop_weak = mainloop.downgrade();
    let shared_ctl = Rc::clone(&shared);
    let _attached = control_rx.attach(mainloop.loop_(), move |msg| match msg {
        Control::Start(sink) => {
            shared_ctl.borrow_mut().sink = Some(sink);
            tracing::debug!("destination des images installee");
        }
        Control::Quit => {
            if let Some(ml) = mainloop_weak.upgrade() {
                ml.quit();
            }
        }
    });

    mainloop.run();

    // Ordre de liberation explicite : ecouteur, puis flux, puis session.
    drop(listener);
    let _ = stream.disconnect();
    drop(stream);
    drop(portal);
    Ok(())
}

/// Callback temps reel : une copie, un envoi non bloquant, rien d'autre.
fn on_process(stream: &pw::stream::Stream, shared: &Rc<RefCell<Shared>>) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        // Tous les tampons sont detenus en aval : la file est saturee. Le
        // compteur correspondant est incremente par le sink.
        return;
    };

    // Horodatage : on privilegie celui du compositeur, pris au moment ou
    // l'image a ete composee. A defaut, l'instant de reception, qui inclut le
    // trajet inter-processus (quelques centaines de microsecondes).
    let header_pts = buffer
        .find_meta::<MetaHeader>()
        .map(|h| h.pts())
        .filter(|pts| *pts > 0);
    let received_ns = monotonic_ns();
    let pts_ns = header_pts.unwrap_or(received_ns);

    let mut s = shared.borrow_mut();
    if header_pts.is_none() && !s.warned_missing_header {
        s.warned_missing_header = true;
        tracing::warn!(
            "le compositeur ne fournit pas d'horodatage : repli sur l'horloge de reception"
        );
    }
    let Some(geom) = s.geometry else { return };

    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else { return };

    let chunk = data.chunk();
    let size = chunk.size() as usize;
    let offset = chunk.offset() as usize;
    let chunk_stride = chunk.stride();
    if size == 0 {
        // Trame vide : le compositeur signale "rien de neuf".
        return;
    }
    let stride = if chunk_stride > 0 {
        chunk_stride as u32
    } else {
        geom.width * geom.format.bytes_per_pixel() as u32
    };

    let needed = geom.format.buffer_size(stride as usize, geom.height as usize);
    let Some(pool) = s.ensure_pool(stride, geom.height, geom.format) else {
        return;
    };
    let pool = Arc::clone(pool);

    let Some(src) = data.data() else {
        // Sans MAP_BUFFERS (ou avec un DMA-BUF), la memoire n'est pas
        // accessible au CPU. On le signale une seule fois.
        if !s.warned_short_buffer {
            s.warned_short_buffer = true;
            tracing::error!("tampon PipeWire non projetable en memoire : format non gere");
        }
        return;
    };
    if offset + needed > src.len() {
        if !s.warned_short_buffer {
            s.warned_short_buffer = true;
            tracing::error!(
                offset,
                needed,
                available = src.len(),
                "tampon PipeWire plus court qu'annonce"
            );
        }
        return;
    }

    let mut dst = pool.acquire();
    dst.as_mut_slice()[..needed].copy_from_slice(&src[offset..offset + needed]);

    let frame = VideoFrame::new(
        dst,
        geom.width,
        geom.height,
        stride,
        geom.format,
        pts_ns,
        received_ns,
    );

    if let Some(sink) = s.sink.as_ref() {
        sink.stats().record_capture_latency_us(frame.capture_latency_us());
        if !sink.submit(frame) {
            s.sink = None;
        }
    }
}

fn map_video_format(f: VideoFormat) -> Option<PixelFormat> {
    match f {
        VideoFormat::BGRx => Some(PixelFormat::Bgrx),
        VideoFormat::BGRA => Some(PixelFormat::Bgra),
        VideoFormat::RGBx => Some(PixelFormat::Rgbx),
        VideoFormat::RGBA => Some(PixelFormat::Rgba),
        _ => None,
    }
}

fn serialize_object(obj: &spa::pod::Object) -> Option<Vec<u8>> {
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj.clone()),
    )
    .ok()
    .map(|(cursor, _)| cursor.into_inner())
}

/// Liste des formats acceptes, du plus economique au moins souhaitable.
///
/// `BGRx` est place en premier : c'est le format natif de la quasi-totalite
/// des compositeurs, donc celui qui evite une conversion cote compositeur.
/// Le framerate maximal est fixe a la cible : inutile de faire travailler le
/// compositeur plus vite que ce que l'on encodera.
fn build_enum_format(fps: u32) -> Vec<u8> {
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: fps, denom: 1 }
        ),
    );
    serialize_object(&obj).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_format_pod_is_well_formed() {
        let bytes = build_enum_format(60);
        assert!(!bytes.is_empty(), "le pod ne doit pas etre vide");
        assert!(
            Pod::from_bytes(&bytes).is_some(),
            "le pod doit etre relisible par libspa"
        );
    }

    #[test]
    fn video_formats_are_mapped_to_our_own_enum() {
        assert_eq!(map_video_format(VideoFormat::BGRx), Some(PixelFormat::Bgrx));
        assert_eq!(map_video_format(VideoFormat::BGRA), Some(PixelFormat::Bgra));
        assert_eq!(map_video_format(VideoFormat::RGBA), Some(PixelFormat::Rgba));
        // Un format planaire n'est pas accepte par ce chemin.
        assert_eq!(map_video_format(VideoFormat::I420), None);
    }

    #[test]
    fn pool_is_sized_from_the_real_stride() {
        let (tx, _rx) = bounded(1);
        let mut s = Shared {
            cfg_pool_size: 4,
            sink: None,
            pool: None,
            geometry: None,
            stride: 0,
            announced: false,
            ready: tx,
            warned_missing_header: false,
            warned_short_buffer: false,
        };
        // Pas de ligne aligne, superieur a width * 4.
        let pool = s.ensure_pool(2624 * 4, 1440, PixelFormat::Bgrx);
        assert!(pool.is_some());
        assert_eq!(s.pool.as_ref().map(|p| p.buffer_size()), Some(2624 * 4 * 1440));

        // Un changement de geometrie reconstruit le pool.
        s.ensure_pool(1920 * 4, 1080, PixelFormat::Bgrx);
        assert_eq!(s.pool.as_ref().map(|p| p.buffer_size()), Some(1920 * 4 * 1080));
    }
}
