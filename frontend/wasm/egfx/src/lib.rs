//! The page's compositor for an RDP host's graphics pipeline.
//!
//! In a session started with the pipeline passed the gateway passes its commands
//! to the browser instead of composing them and encoding the picture. This is what
//! composes them there: the gateway's own compositor and codecs, which are the
//! `remotex-rdp-graphics` crate both are built with, behind the few calls the paint
//! worker makes (`frontend/src/egfxCompositor.ts`).
//!
//! The framebuffer stays in this module's memory, which is shared with the threads
//! below: the page uploads the rectangles a run painted out of it, into a texture.
//!
//! H.264 is the one codec not decoded here: the page decodes a run's access units
//! with the browser's own decoder before it composes the run, and hands each
//! picture's samples into this memory ([`Egfx::units`], [`Egfx::reserve`],
//! [`Egfx::supply`]).
//!
//! Progressive's tiles are decoded side by side, on rayon's pool, and a page has no
//! threads to spawn: a thread is a worker the page starts, running an instance of
//! this module on the one memory. So the pool's threads are seats the page's workers
//! take ([`run_pool_thread`]) before the pool is made of them ([`start_pool`]). A
//! module whose pool was never started composes on the thread that calls it.

use std::io;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, OnceLock};

use rayon::{ThreadBuilder, ThreadPoolBuilder};
use remotex_rdp_graphics::Compositor;
use remotex_rdp_graphics::avc::{self, Picture, Plane, Samples, Scanned, Window};
use wasm_bindgen::prelude::*;

/// The pool's threads on their way from the pool that makes them to the workers
/// that run them.
struct Seats {
    offer: Mutex<Sender<ThreadBuilder>>,
    take: Mutex<Receiver<ThreadBuilder>>,
}

fn seats() -> &'static Seats {
    static SEATS: OnceLock<Seats> = OnceLock::new();
    SEATS.get_or_init(|| {
        let (offer, take) = channel();
        Seats { offer: Mutex::new(offer), take: Mutex::new(take) }
    })
}

/// The compiled module, for a worker to make its instance of.
#[wasm_bindgen]
pub fn module() -> JsValue {
    wasm_bindgen::module()
}

/// Run one of the pool's threads on the worker that calls this, which waits for
/// the pool to be started and returns when the pool is gone: never, in a page.
#[wasm_bindgen(js_name = runPoolThread)]
pub fn run_pool_thread() {
    let thread = seats().take.lock().expect("no thread panics holding a seat").recv();
    if let Ok(thread) = thread {
        thread.run();
    }
}

/// Make the pool, of `threads` workers that are each in [`run_pool_thread`]
/// already: this waits for every thread to have started, and a worker cannot start
/// while the thread that made it waits.
#[wasm_bindgen(js_name = startPool)]
pub fn start_pool(threads: usize) -> Result<(), JsError> {
    let offer = seats().offer.lock().expect("no thread panics holding a seat").clone();
    ThreadPoolBuilder::new()
        .num_threads(threads)
        // The thread that was not sent is dropped with the error: it is not `Sync`,
        // which an error is.
        .spawn_handler(move |thread| offer.send(thread).map_err(|_| io::Error::other("the pool's seats are gone")))
        .build_global()
        .map_err(|e| JsError::new(&e.to_string()))
}

/// One pipeline's compositor. Made where a pipeline starts (`graphicsStart`) and
/// thrown away where the next one does: it is right only for a pipeline it has
/// followed from its first command.
#[wasm_bindgen]
pub struct Egfx {
    compositor: Compositor,
    /// What the last run painted: `x, y, width, height` for each rectangle.
    painted: Vec<u32>,
    resized: bool,
    /// Where a decoded picture's samples land on their way to the compositor:
    /// reserved, written by the page, then supplied.
    landing: Vec<u8>,
}

/// How many numbers [`Egfx::units`] says each thing it found in.
const UNIT_FIELDS: usize = 10;

#[wasm_bindgen]
impl Egfx {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Egfx {
        Egfx { compositor: Compositor::new(), painted: Vec::new(), resized: false, landing: Vec::new() }
    }

    /// One `GRAPHICS` record's message, as the host sent it, out of its bulk
    /// compression: the commands [`Self::units`] and [`Self::compose`] take. Every
    /// record of the pipeline comes through here, in order, since each is
    /// compressed against the ones before it.
    ///
    /// Throws for a message that does not unwrap, after which none after it can be.
    pub fn unwrap(&mut self, wrapped: &[u8]) -> Result<Vec<u8>, JsError> {
        self.compositor.unwrap(wrapped).map_err(|e| JsError::new(&format!("{e:#}")))
    }

    /// Compose one `GRAPHICS` record's commands, unwrapped, and return how many
    /// frames they ended. What they painted is [`Self::painted`] until the next call.
    ///
    /// Throws for a command that does not decode. The compositor is then no longer
    /// the host's picture of its client, and is not to be fed again.
    pub fn compose(&mut self, commands: &[u8]) -> Result<u32, JsError> {
        self.painted.clear();
        self.resized = false;
        let composed = self.compositor.compose(commands).map_err(|e| JsError::new(&format!("{e:#}")))?;
        self.resized = composed.resized.is_some();
        for rect in composed.painted {
            self.painted.extend([rect.x, rect.y, rect.width, rect.height]);
        }
        Ok(composed.frames)
    }

    /// What a `GRAPHICS` record's commands hold for the page to act on before it
    /// composes them, in command order, ten numbers each.
    ///
    /// An H.264 access unit: `0`, its surface, where its bytes start and end in the
    /// commands, whether a decoder can start at it, the profile, constraint and
    /// level bytes of its parameter sets as `0x01PPCCLL` or zero where it has none,
    /// and the part of its picture to supply — `left, top, right, bottom`, all
    /// zero for none of it and all `0xFFFFFFFF` for the whole. A unit's number, for
    /// [`Self::supply`], is how many units the record has before it.
    ///
    /// A surface whose stream is over: `1`, the surface, and zeros.
    ///
    /// Throws for a command that does not decode, which [`Self::compose`] would
    /// throw for too.
    pub fn units(&self, commands: &[u8]) -> Result<Vec<u32>, JsError> {
        let found = avc::scan(commands).map_err(|e| JsError::new(&e.to_string()))?;
        let mut out = Vec::with_capacity(found.len() * UNIT_FIELDS);
        for found in found {
            let fields: [u32; UNIT_FIELDS] = match found {
                Scanned::Gone { surface } => [1, surface.into(), 0, 0, 0, 0, 0, 0, 0, 0],
                Scanned::Unit(unit) => {
                    let profile = unit.profile.map_or(0, |[p, c, l]| u32::from_be_bytes([1, p, c, l]));
                    let [left, top, right, bottom] = match unit.window {
                        Window::Nothing => [0; 4],
                        Window::Part(rect) => [rect.left, rect.top, rect.right, rect.bottom].map(u32::from),
                        Window::Whole => [u32::MAX; 4],
                    };
                    // A record is a WebSocket message, far short of four gigabytes.
                    let (start, end) = (unit.start as u32, unit.end as u32);
                    [0, unit.surface.into(), start, end, unit.key.into(), profile, left, top, right, bottom]
                }
            };
            out.extend(fields);
        }
        Ok(out)
    }

    /// Room for one decoded picture's samples, `bytes` of them, in this module's
    /// memory: where the page has its decoder copy them. Good until the next call
    /// here or to [`Self::supply`].
    pub fn reserve(&mut self, bytes: usize) -> *mut u8 {
        self.landing = vec![0; bytes];
        self.landing.as_mut_ptr()
    }

    /// Supply the picture just written into [`Self::reserve`]'s room as that of
    /// access unit `unit` of the record composed next.
    ///
    /// The picture holds `width` by `height` pixels from `left`, `top` of the
    /// decoder's, and `layout` is where each of its planes starts in the room and
    /// the bytes between its rows: three pairs for a decoder that hands back Y, U
    /// and V apart (`I420`), two for one that interleaves the chroma (`NV12`).
    pub fn supply(
        &mut self,
        unit: u32,
        left: u32,
        top: u32,
        width: u32,
        height: u32,
        layout: &[u32],
    ) -> Result<(), JsError> {
        let plane = |at: usize| Plane { offset: layout[at] as usize, stride: layout[at + 1] as usize };
        let samples = match layout.len() {
            6 => Samples::I420 { y: plane(0), u: plane(2), v: plane(4) },
            4 => Samples::Nv12 { y: plane(0), uv: plane(2) },
            other => return Err(JsError::new(&format!("a picture laid out in {other} numbers"))),
        };
        let data = std::mem::take(&mut self.landing);
        self.compositor.supply(unit, Picture { data, samples, left, top, width, height });
        Ok(())
    }

    /// The rectangles the last run painted, four numbers each: `x, y, width,
    /// height`, in framebuffer pixels.
    pub fn painted(&self) -> Vec<u32> {
        self.painted.clone()
    }

    /// Whether the last run reset the output, which leaves the framebuffer at its
    /// new size and blank but for what the run painted after.
    pub fn resized(&self) -> bool {
        self.resized
    }

    pub fn width(&self) -> u32 {
        self.compositor.framebuffer().with(|frame| frame.width)
    }

    pub fn height(&self) -> u32 {
        self.compositor.framebuffer().with(|frame| frame.height)
    }

    /// Where the framebuffer is in this module's memory: `width * height * 4`
    /// bytes, `RGBX` — the fourth byte unused — top row first. Good until the next
    /// [`Self::compose`], which may move it.
    pub fn pixels(&self) -> *const u8 {
        self.compositor.framebuffer().with(|frame| frame.pixels.as_ptr())
    }
}

impl Default for Egfx {
    fn default() -> Self {
        Self::new()
    }
}
