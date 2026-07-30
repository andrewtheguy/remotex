//! ScreenCaptureKit dirty rectangles in, packed-RGB tiles out.
//!
//! Pixel reads honor the buffer's reported row stride. Capture stays at native
//! pixel size and reports the measured backing scale for point-based input.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use log::{debug, info, warn};
use screencapturekit::stream::delegate_trait::ErrorHandler;
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;

/// Stable surface-pixel grid shared with the gateway. Dirty rectangles expand
/// outward to cell boundaries so hashing and cache identities remain reusable.
const CELL_W: u16 = 320;
const CELL_H: u16 = 64;

/// A rectangle in captured-surface pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// One tile's worth of changed screen: a rectangle plus packed RGB888.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTile {
    pub rect: Rect,
    /// `w * h * 3` bytes, row-major, no padding.
    pub rgb: Vec<u8>,
}

/// Where captured tiles go. Implemented by the session pipeline.
///
/// Called from ScreenCaptureKit's dispatch queue — an arbitrary thread, possibly
/// several concurrently — so implementations must be cheap and must not block.
pub trait FrameSink: Send + Sync + 'static {
    /// One frame's changed tiles. Empty vectors are never sent.
    fn tiles(&self, tiles: Vec<RawTile>);
    /// The captured surface size changed (display reconfigured, mode switch).
    fn resized(&self, w: u16, h: u16);
    /// The stream cannot continue. The session decides whether to restart it.
    fn failed(&self, message: String);
}

/// Display selected by stable CoreGraphics id. Owned displays also retain their
/// immutable creation envelope for scale fallback and resize clamping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    /// One of the Mac's own displays, by CoreGraphics display id.
    Real(u32),
    /// Agent-created display and its creation size in points.
    Owned { id: u32, base_points: (u32, u32) },
}

impl Target {
    /// The display id this target names, whichever kind it is.
    pub fn id(self) -> u32 {
        match self {
            Target::Real(id) | Target::Owned { id, .. } => id,
        }
    }

    /// The target as [`pick`] actually resolved it, given the display the stream
    /// ended up on.
    ///
    /// A real display that has been unplugged falls back to the main one, so the
    /// id asked for and the id being captured can differ — and what a client is
    /// told is active has to be the one that is true. An owned target never
    /// falls back, so it is returned unchanged.
    pub fn resolved(self, id: u32) -> Self {
        match self {
            Target::Real(_) => Target::Real(id),
            owned => owned,
        }
    }
}

/// The Mac's main display — where every session starts, and what an unresolvable
/// selection falls back to.
pub fn main_display() -> u32 {
    unsafe { CGMainDisplayID() }
}

/// Backing scale of a display *this process created*. Measured from pixels, with
/// the creation envelope as the fallback.
///
/// Not from the mode, which is what this used to do and is the bug it exists to
/// document. A `CGVirtualDisplay`'s owner is told a different mode than every other
/// process: measured on the test VM, with the display's framebuffer provably
/// 3200x2000, this process read `CGDisplayCopyDisplayMode` as 1600x1000 pixels
/// (ioDisplayModeID 11) while a freshly started process read 3200x2000
/// (ioDisplayModeID 10), `screencapture` wrote a 3200x2000 PNG, and
/// `NSScreen.backingScaleFactor` — which AppKit derives from the same mode — read
/// 1 here and 2 there. It is not a stale cache: display reconfiguration callbacks
/// arrive in this process and the reading does not change after them.
///
/// So the density of our own display had to be read some other way, and pixels are
/// the way ([`framebuffer_scale`]). Everything downstream of a wrong answer here is
/// wrong in the same direction: the capture surface is sized `points * scale`, so a
/// 2x desktop read as 1x is captured at half its resolution and looks soft in every
/// client, the wire reports "1600×1000 at 1x", and
/// [`crate::virtualdisplay::VirtualDisplay::set_scale`] cannot tell whether it has
/// anything to do.
fn owned_display_scale(id: u32, points: (f64, f64), base: (u32, u32)) -> f64 {
    framebuffer_scale(id).unwrap_or_else(|| owned_scale(points, base))
}

/// Pixels per point, measured by capturing one point of the display.
///
/// The one reading of an owned display's density that is true in the process that
/// created it — see [`owned_display_scale`] for what is wrong with the others. A
/// one-point rect comes back as a 2x2 image on a 2x display and 1x1 on a 1x one,
/// which is the whole measurement; the pixels themselves are thrown away.
///
/// `CGDisplayCreateImageForRect` is deprecated in favour of ScreenCaptureKit, and
/// there is no ScreenCaptureKit equivalent: SCK reports points, and the surface size
/// of a stream is whatever it was configured with — which is the number being
/// derived here, so asking it would be circular. It costs about 13 ms on the test
/// VM, which is why it is called where the density can have changed (a capture
/// starting, the 2-second display poll, a reconfigure) and not per frame.
///
/// `None` when the capture fails: mid-reconfigure, or without the Screen Recording
/// grant — and without that grant there is no capture to size anyway.
#[allow(deprecated)]
pub(crate) fn framebuffer_scale(id: u32) -> Option<f64> {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{CGDisplayCreateImageForRect, CGImage};

    let point = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1.0, 1.0));
    let image = CGDisplayCreateImageForRect(id, point)?;
    let pixels = CGImage::width(Some(&image));
    (pixels > 0).then_some(pixels as f64)
}

/// Degraded scale estimate from the immutable creation envelope. It cannot
/// distinguish same-size 1x and 2x modes, so callers must prefer
/// [`framebuffer_scale`].
fn owned_scale(points: (f64, f64), base: (u32, u32)) -> f64 {
    if points.0 <= f64::from(base.0) && points.1 <= f64::from(base.1) {
        crate::virtualdisplay::SCALE
    } else {
        1.0
    }
}

/// What the agent needs to know about a display before it starts streaming:
/// the size to announce in `Hello`, and the two numbers input conversion needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    /// The CoreGraphics display this was measured from. Carried along because
    /// the display index the config names is resolved here (see [`pick`]), so
    /// without it a caller could not tell which display it ended up with.
    pub id: u32,
    /// Captured surface size in pixels.
    pub width: u16,
    pub height: u16,
    /// Captured pixels per display point — 2.0 on a Retina panel. Input
    /// coordinates are divided by this (see [`crate::input`]).
    pub scale: f64,
    /// The display's origin in the global point coordinate space, which
    /// `CGEventPost` addresses. Non-zero for a secondary display.
    pub origin: (f64, f64),
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Whether Screen Recording is granted, checked *without* prompting.
    fn CGPreflightScreenCaptureAccess() -> bool;
    /// Raise the Screen Recording prompt if it has not been answered yet.
    fn CGRequestScreenCaptureAccess() -> bool;
    /// The display the menu bar is on. Needs no Screen Recording grant.
    fn CGMainDisplayID() -> u32;
}

/// Whether Screen Recording is granted, checked without prompting.
pub fn screen_recording_granted() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Ask for Screen Recording, raising the system prompt if it is unanswered.
///
/// This has to be an explicit call. `SCShareableContent::get` does **not**
/// prompt — it just fails with "the user declined TCCs", which is
/// indistinguishable from an actual refusal and leaves the agent absent from
/// the Screen Recording list entirely, so there is nothing for the user to
/// switch on. Calling this at startup is what puts it there.
///
/// Returns the resulting grant state. Once the user has answered, macOS
/// remembers it and this stops prompting — so it is safe on every launch.
pub fn request_screen_recording() -> bool {
    unsafe { CGRequestScreenCaptureAccess() }
}

/// Measure a display without starting a stream.
///
/// `Hello` has to carry the screen size before the gateway sends `Attach`, but
/// the stream itself should not start until it does (battery, CPU) — so the
/// geometry is probed separately. This still needs the Screen Recording grant,
/// which makes it the natural place to discover a missing one.
pub fn probe(target: Target) -> anyhow::Result<Geometry> {
    let content = SCShareableContent::get()
        .map_err(|e| anyhow::anyhow!("cannot list shareable content: {e}"))?;
    let displays = content.displays();
    anyhow::ensure!(!displays.is_empty(), "no displays available to capture");
    Ok(geometry(pick(&displays, target)?, target))
}

/// One display the agent could share, named the way both clients and the
/// settings dialog show it.
///
/// The two strings are built here rather than in each client so a menu item in
/// the viewer, a row in the browser panel and a line in the agent's own settings
/// all read the same, and so that nothing outside this module has to know how
/// macOS numbers displays.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    /// Short enough for a menu item: `"Display 2"`, or `"Virtual display"`.
    pub label: String,
    /// The line under it: `"1600×1000 at 2x"`.
    pub detail: String,
    /// The CoreGraphics display id lives in [`Geometry::id`], so two identical
    /// panels are still tellable apart.
    pub geometry: Geometry,
    /// The Mac's main display — where a session starts.
    pub is_main: bool,
    /// The display the agent created for itself.
    pub is_owned: bool,
}

impl DisplayInfo {
    /// `"Display 2 — 1440×900 at 1x"`, matching the viewer's menu item.
    pub fn summary(&self) -> String {
        format!("{} — {}", self.label, self.detail)
    }
}

/// The displays the agent could share.
///
/// `owned` is the display the agent created, if it created one; it is in this
/// list like any other display, but only the caller knows which id is ours and
/// [`owned_display_scale`] is the only way to measure it correctly.
///
/// Needs the Screen Recording grant like everything else in this module, so a
/// caller that gets an error here should say so rather than showing an empty
/// list — "no displays" and "not allowed to look" are very different problems.
pub fn displays(owned: Option<Target>) -> anyhow::Result<Vec<DisplayInfo>> {
    let content = SCShareableContent::get()
        .map_err(|e| anyhow::anyhow!("cannot list shareable content: {e}"))?;
    let displays = content.displays();
    anyhow::ensure!(!displays.is_empty(), "no displays available to capture");
    let main = main_display();
    let owned_id = owned.map(Target::id);
    // Numbered over the Mac's own screens only, so the agent's display appearing
    // among them does not shift what "Display 2" means.
    let mut number = 0;
    let listed: Vec<DisplayInfo> = displays
        .iter()
        .map(|display| {
            let id = display.display_id();
            let is_owned = owned_id == Some(id);
            let target = match owned {
                Some(target) if is_owned => target,
                _ => Target::Real(id),
            };
            let geometry = geometry(display, target);
            let label = if is_owned {
                "Virtual display".to_owned()
            } else {
                number += 1;
                format!("Display {number}")
            };
            DisplayInfo {
                detail: detail(&geometry),
                label,
                geometry,
                is_main: id == main,
                is_owned,
            }
        })
        .collect();
    Ok(listed)
}

/// The line a client shows under a display's name: its size in **points**, and
/// the scale those points are drawn at.
///
/// Points rather than the captured pixels [`Geometry`] carries: a 2x display
/// would otherwise be listed at twice the size System Settings calls it —
/// "3200×2400 at 2x" for the display macOS, the settings dialog and the Displays
/// pane all agree is 1600×1200 — which reads as a resolution nobody chose. The
/// pixel count is not lost, it is the product of the two numbers shown.
fn detail(geometry: &Geometry) -> String {
    let scale = if geometry.scale > 0.0 {
        geometry.scale
    } else {
        1.0
    };
    format!(
        "{}×{} at {}x",
        (f64::from(geometry.width) / scale).round() as u32,
        (f64::from(geometry.height) / scale).round() as u32,
        // Trims 2.0 to "2" while leaving a fractional scale readable.
        (scale * 100.0).round() / 100.0
    )
}

/// A running capture stream. Dropping this stops the capture.
pub struct Capture {
    stream: SCStream,
    shared: Arc<Shared>,
    /// The geometry the stream was configured for.
    pub geometry: Geometry,
    /// How to re-measure that display — see [`geometry_for_target`].
    target: Target,
}

/// How many frames are logged at `info` after a stream starts.
///
/// "The stream started but nothing painted" is otherwise invisible: every
/// reason a frame is skipped is individually unremarkable, and at `debug` the
/// evidence never reaches a log anyone reads. A handful of lines per session
/// answers "is ScreenCaptureKit delivering anything, and what?" immediately.
const FRAMES_TO_LOG: u64 = 3;

/// Last 64-bit pixel digest sent for each stable cell. Hashing before encoding
/// removes ScreenCaptureKit's coarse false-positive damage.
#[derive(Default)]
struct CellMemo {
    sent: std::collections::HashMap<(u16, u16, u16, u16), u64>,
}

impl CellMemo {
    /// Whether this cell's pixels differ from the last ones sent for it, recording
    /// them either way.
    fn is_new(&mut self, pixels: &[u8], stride: usize, rect: Rect) -> bool {
        let Some(digest) = cell_digest(pixels, stride, rect) else {
            // The cell does not fit the surface — a bug elsewhere. Send it rather
            // than remember a digest of something that was never sent.
            return true;
        };
        let key = (rect.x, rect.y, rect.w, rect.h);
        self.sent.insert(key, digest) != Some(digest)
    }

    /// Forget everything, because a full repaint is about to be sent.
    ///
    /// Every path that empties a viewer's canvas arrives here through
    /// `full_repaint`: the first frame after `Attach`, a `Refresh` from the
    /// gateway, a surface resize, and the sink dropping a frame it could not
    /// queue. That is what makes this one call enough.
    fn forget(&mut self) {
        self.sent.clear();
    }
}

/// Hash a cell's source pixels, or `None` if it does not fit the surface.
///
/// Reads the BGRA rows in place: the point is to answer "did this change?" without
/// paying for the RGB pack, so this must not copy. The dimensions go into the
/// digest too, so two differently-shaped cells cannot match by hashing the same
/// bytes.
fn cell_digest(pixels: &[u8], stride: usize, rect: Rect) -> Option<u64> {
    let w = usize::from(rect.w);
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(&rect.w.to_le_bytes());
    hasher.update(&rect.h.to_le_bytes());
    for row in 0..usize::from(rect.h) {
        let start = (usize::from(rect.y) + row) * stride + usize::from(rect.x) * 4;
        hasher.update(pixels.get(start..start + w * 4)?);
    }
    Some(hasher.digest())
}

/// State shared with the capture callback.
struct Shared {
    sink: Arc<dyn FrameSink>,
    /// Frames the callback has been handed, for [`FRAMES_TO_LOG`].
    frames_seen: std::sync::atomic::AtomicU64,
    /// Set to make the next frame report the whole surface as dirty. Used for
    /// the first frame after `Attach`, for `Refresh`, and to recover from a
    /// dropped backlog. Shared with the session — see [`Capture::start`].
    full_repaint: Arc<AtomicBool>,
    /// Last surface size seen, so a change is reported once rather than per
    /// frame.
    last_size: Mutex<(u16, u16)>,
    /// See [`CellMemo`]. Locked once per frame, around the cell loop.
    cells: Mutex<CellMemo>,
}

impl Capture {
    /// Start capturing `display` (an index into the shareable-display list).
    ///
    /// `full_repaint` is shared with the caller: setting it makes the next frame
    /// report the whole surface as dirty. The session sets it on `Refresh`, and
    /// the sink sets it when it has to drop a frame — which is how falling
    /// behind degrades into a later, coarser repaint instead of a backlog.
    ///
    /// It starts set, so the first frame is a complete picture and a freshly
    /// attached browser does not wait for the screen to change.
    pub fn start(
        target: Target,
        sink: Arc<dyn FrameSink>,
        full_repaint: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        // Fails with "user declined TCC" when Screen Recording is not granted;
        // the caller turns that into an AgentMsg::Error for the browser.
        let content = SCShareableContent::get()
            .map_err(|e| anyhow::anyhow!("cannot list shareable content: {e}"))?;
        let displays = content.displays();
        anyhow::ensure!(!displays.is_empty(), "no displays available to capture");
        let display = pick(&displays, target)?;
        let geometry = geometry(display, target);
        info!(
            "capture: display {} — {}x{} pixels at {}x, origin ({}, {})",
            display.display_id(),
            geometry.width,
            geometry.height,
            geometry.scale,
            geometry.origin.0,
            geometry.origin.1
        );

        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();

        full_repaint.store(true, Ordering::Relaxed);
        let config = stream_config(geometry.width, geometry.height);

        let shared = Arc::new(Shared {
            sink,
            frames_seen: std::sync::atomic::AtomicU64::new(0),
            full_repaint,
            last_size: Mutex::new((geometry.width, geometry.height)),
            cells: Mutex::new(CellMemo::default()),
        });

        // The delegate is how ScreenCaptureKit reports an *unexpected* stop —
        // the display going away, the Screen Recording grant being revoked mid
        // session, the system tearing the stream down. Without it those look
        // identical to "the screen stopped changing", and the session would sit
        // there painting nothing forever.
        let delegate_shared = Arc::clone(&shared);
        let mut stream = SCStream::new_with_delegate(
            &filter,
            &config,
            ErrorHandler::new(move |error| {
                delegate_shared.sink.failed(format!("capture stream stopped: {error}"));
            }),
        );
        stream.add_output_handler(Handler(Arc::clone(&shared)), SCStreamOutputType::Screen);
        stream
            .start_capture()
            .map_err(|e| anyhow::anyhow!("cannot start the capture stream: {e}"))?;

        Ok(Self {
            stream,
            shared,
            geometry,
            target,
        })
    }

    /// Make the next frame report the whole surface as dirty.
    pub fn request_full_repaint(&self) {
        self.shared.full_repaint.store(true, Ordering::Relaxed);
    }

    /// Reconfigure the fixed-size SCStream surface after a display mode change.
    /// The next frame uses the ordinary resize notification path. `None` means
    /// unchanged or temporarily unreadable while the display settles.
    pub fn follow_display(&mut self) -> anyhow::Result<Option<Geometry>> {
        let Some(live) = geometry_for_target(self.target, self.geometry.id) else {
            debug!(
                "capture: display {} reports no mode; mid-reconfigure",
                self.geometry.id
            );
            return Ok(None);
        };
        if (live.width, live.height) == (self.geometry.width, self.geometry.height) {
            return Ok(None);
        }
        info!(
            "capture: display {} is now {}x{} at {}x; resizing the capture surface",
            live.id, live.width, live.height, live.scale
        );
        self.stream
            .update_configuration(&stream_config(live.width, live.height))
            .map_err(|e| anyhow::anyhow!("cannot resize the capture surface: {e}"))?;
        self.geometry = live;
        // The whole surface is new. Without this the first frame at the new size
        // would carry only the rectangles that happened to change since.
        self.shared.full_repaint.store(true, Ordering::Relaxed);
        Ok(Some(live))
    }

    /// Stop the stream. Also happens on drop; this exists so a session can stop
    /// capturing (battery, CPU) while keeping the object around.
    pub fn stop(&mut self) {
        if let Err(e) = self.stream.stop_capture() {
            debug!("capture: stop_capture: {e}");
        }
    }
}

/// The stream settings, which are the same at start and after a resize — only
/// the surface dimensions differ, so they are the parameters.
fn stream_config(width: u16, height: u16) -> SCStreamConfiguration {
    SCStreamConfiguration::new()
        .with_width(u32::from(width))
        .with_height(u32::from(height))
        .with_pixel_format(PixelFormat::BGRA)
        // The pointer is sent as a shape instead (see crate::cursor), so it
        // must not be burned into the framebuffer.
        .with_shows_cursor(false)
        // Cap the frame rate: past ~30fps the encoder is the bottleneck and
        // the extra frames only add latency.
        .with_minimum_frame_interval(&CMTime::new(1, 30))
        // Shallow: a deep queue buys buffering we do not want. Falling
        // behind should coalesce into a later, coarser repaint.
        .with_queue_depth(3)
}

/// Re-measure a target the way its kind allows.
///
/// The owned case does not go through [`geometry_for_id`], and cannot: that path
/// takes both the size and the density from the display's mode, which is the one
/// thing about a display we created that this process is told wrongly (see
/// [`owned_display_scale`]). Bounds are right for either kind, so they drive the
/// size, and the density is measured from pixels.
///
/// The absence of a reading is also handled differently here. `geometry_for_id`
/// returns `None` for "mid-reconfigure, try again"; for a display of our own that is
/// a state to degrade in rather than stall in, since bounds *are* always published
/// and [`owned_display_scale`] falls back to the creation envelope.
pub fn geometry_for_target(target: Target, id: u32) -> Option<Geometry> {
    use objc2_core_graphics::CGDisplayBounds;

    let Target::Owned { base_points, .. } = target else {
        return geometry_for_id(id);
    };
    let bounds = CGDisplayBounds(id);
    let (points_w, points_h) = (bounds.size.width, bounds.size.height);
    if points_w <= 0.0 || points_h <= 0.0 {
        return None;
    }
    let scale = owned_display_scale(id, (points_w, points_h), base_points);
    Some(Geometry {
        id,
        width: clamp_u16((points_w * scale).round() as u32),
        height: clamp_u16((points_h * scale).round() as u32),
        scale,
        origin: (bounds.origin.x, bounds.origin.y),
    })
}

/// Measure a display through CoreGraphics rather than ScreenCaptureKit.
///
/// [`geometry`] needs an `SCDisplay`, and getting one means `SCShareableContent`
/// — a round trip to a system service, too heavy to run on a poll. Everything in
/// [`Geometry`] is available from CoreGraphics by display id, so a live
/// re-measure of a display already being captured comes from here.
///
/// `None` while the display has no mode at all, which is what CoreGraphics
/// briefly reports mid-reconfigure.
pub fn geometry_for_id(id: u32) -> Option<Geometry> {
    use objc2_core_graphics::{CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode};

    let mode = CGDisplayCopyDisplayMode(id)?;
    let pixel_w = CGDisplayMode::pixel_width(Some(&mode));
    let pixel_h = CGDisplayMode::pixel_height(Some(&mode));
    let point_w = CGDisplayMode::width(Some(&mode));
    if pixel_w == 0 || pixel_h == 0 {
        return None;
    }
    // From the same mode rather than via `display_scale`, which would copy the
    // mode a second time — this runs on a poll.
    let scale = if point_w > 0 {
        pixel_w as f64 / point_w as f64
    } else {
        1.0
    };
    let bounds = CGDisplayBounds(id);
    Some(Geometry {
        id,
        width: clamp_u16(pixel_w as u32),
        height: clamp_u16(pixel_h as u32),
        scale,
        origin: (bounds.origin.x, bounds.origin.y),
    })
}

/// Resolve a target against the shareable list.
///
/// A *real* display that has gone falls back to the main one rather than failing
/// — a screen unplugged mid-session should degrade to a working session, and the
/// client learns which display it landed on from the next `Displays`. A missing
/// *owned* display is an error instead: falling back there would share the screen
/// of whoever is sitting at the Mac, having been asked for a private desktop,
/// which is the one substitution nobody would want made silently.
fn pick(displays: &[SCDisplay], target: Target) -> anyhow::Result<&SCDisplay> {
    match target {
        Target::Real(id) => Ok(displays
            .iter()
            .find(|display| display.display_id() == id)
            .unwrap_or_else(|| {
                warn!("capture: display {id} is not attached; using the main display");
                let main = main_display();
                displays
                    .iter()
                    .find(|display| display.display_id() == main)
                    .unwrap_or(&displays[0])
            })),
        Target::Owned { id, .. } => displays
            .iter()
            .find(|display| display.display_id() == id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the virtual display ({id}) is not in the shareable list — it may not have \
                     been published yet"
                )
            }),
    }
}

/// Measure a display: `SCDisplay` reports points, and a Retina panel captures at
/// a multiple of that. We ask for the full pixel size so the browser gets the
/// panel's real detail, and carry the scale so input can be converted back.
fn geometry(display: &SCDisplay, target: Target) -> Geometry {
    let scale = match target {
        // Measured from pixels rather than read like a real display's, with the
        // created size only as a fallback: the mode this process is shown for a
        // display it created describes the mode it *asked for*, not the one macOS
        // is scanning out (see `owned_display_scale`).
        Target::Owned { base_points, .. } => owned_display_scale(
            display.display_id(),
            (display.width() as f64, display.height() as f64),
            base_points,
        ),
        Target::Real(_) => backing_scale(display),
    };
    let frame = display.frame();
    Geometry {
        id: display.display_id(),
        width: clamp_u16(((display.width() as f64) * scale).round() as u32),
        height: clamp_u16(((display.height() as f64) * scale).round() as u32),
        scale,
        origin: (frame.origin.x, frame.origin.y),
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Captured pixels per display point.
///
/// `SCDisplay` exposes points only, so the scale comes from the CoreGraphics
/// display mode: pixel width over point width. A `1.0` fallback is safe — it
/// just means a non-Retina capture.
fn backing_scale(display: &SCDisplay) -> f64 {
    display_scale(display.display_id())
}

/// A display's current size in **points**, from its bounds.
///
/// Bounds rather than a mode, because this is asked of a display of our own and
/// bounds are the one reading always published for those. Points is the unit both
/// of that display's settable properties are expressed in:
/// [`crate::virtualdisplay::VirtualDisplay::set_scale`] re-lists the same logical
/// size at a different density so the desktop keeps its layout, and
/// [`crate::virtualdisplay::VirtualDisplay::set_size`] compares against this to
/// know whether it has anything to do at all.
pub(crate) fn display_points(id: u32) -> (u32, u32) {
    use objc2_core_graphics::CGDisplayBounds;

    let bounds = CGDisplayBounds(id);
    (bounds.size.width as u32, bounds.size.height as u32)
}

pub(crate) fn display_scale(id: u32) -> f64 {
    mode_scale(id).unwrap_or(1.0)
}

/// Pixels per point from a display's current CoreGraphics mode, or `None` when
/// it publishes no mode to read.
///
/// The distinction matters for a display of our own, in two places, and both need
/// "no mode" kept apart from "1x": it is the case [`owned_display_scale`] has to
/// fall back for, and the case
/// [`crate::virtualdisplay::VirtualDisplay::set_size`] refuses to guess at rather
/// than resize a display to a density nobody asked for.
pub(crate) fn mode_scale(id: u32) -> Option<f64> {
    use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode};

    let mode = CGDisplayCopyDisplayMode(id)?;
    let pixel_w = CGDisplayMode::pixel_width(Some(&mode)) as f64;
    let point_w = CGDisplayMode::width(Some(&mode)) as f64;
    (point_w > 0.0 && pixel_w > 0.0).then(|| pixel_w / point_w)
}

struct Handler(Arc<Shared>);

impl SCStreamOutputTrait for Handler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, _: SCStreamOutputType) {
        // The callback runs on ScreenCaptureKit's dispatch queue. Everything
        // here is bounded work — extract pixels, hand them off — and never
        // blocks on the network or on an encoder.
        if let Err(e) = self.handle(&sample) {
            debug!("capture: dropped a frame: {e}");
        }
    }
}

impl Handler {
    fn handle(&self, sample: &CMSampleBuffer) -> anyhow::Result<()> {
        // Only some statuses carry pixels. `Idle` means "nothing changed and
        // there is no new IOSurface", which is the common case on a still
        // screen; `Blank`/`Suspended`/`Stopped` have nothing to read either.
        //
        // `Started` — the stream's *first* frame — does carry content, and
        // accepting it is load-bearing: on a screen that then sits still, it is
        // the only content-bearing frame that ever arrives, so rejecting it
        // means the session paints nothing at all and looks hung.
        let nth = self.0.frames_seen.fetch_add(1, Ordering::Relaxed);
        let status = sample.frame_status();
        // The status attachment is an *optimisation*, not a precondition: it
        // lets us skip `Idle` frames without touching the surface. It is not
        // always there — on macOS 26 this reads `None` for every single frame —
        // and treating a missing status as "no pixels" means the session never
        // paints anything at all, which is exactly the bug this comment exists
        // to prevent a future edit from reintroducing.
        //
        // So: skip only on a status that positively says there is nothing to
        // read. Absent or content-bearing, fall through and let the image
        // buffer decide, which is the real test.
        if let Some(status) = status
            && !status.has_content()
        {
            if nth < FRAMES_TO_LOG {
                info!("capture: frame {nth} status {status:?} — no pixels, skipping");
            }
            anyhow::bail!("frame status {status:?} carries no pixels");
        }

        let buffer = sample
            .image_buffer()
            .ok_or_else(|| anyhow::anyhow!("frame carries no image buffer"))?;
        let guard = buffer
            .lock(CVPixelBufferLockFlags::READ_ONLY)
            .map_err(|e| anyhow::anyhow!("cannot lock the pixel buffer: {e}"))?;

        let stride = guard.bytes_per_row();
        let surface_w = clamp_u16(guard.width() as u32);
        let surface_h = clamp_u16(guard.height() as u32);
        let pixels = guard.as_slice();

        // A mode switch changes the surface under us; tell the session so it can
        // re-announce the size before any tile with new coordinates arrives.
        //
        // The notification happens *outside* the lock: `resized` is the one sink
        // call that may block (a resize must not be dropped, so it does a
        // blocking send), and holding `last_size` across it would park every
        // other dispatch-queue callback on the mutex behind it.
        let resized = {
            let mut last = self.0.last_size.lock().unwrap();
            let changed = *last != (surface_w, surface_h);
            if changed {
                *last = (surface_w, surface_h);
                self.0.full_repaint.store(true, Ordering::Relaxed);
            }
            changed
        };
        if resized {
            self.0.sink.resized(surface_w, surface_h);
        }

        // `swap` so a concurrent callback doesn't also do the full frame.
        let full = self.0.full_repaint.swap(false, Ordering::Relaxed);
        let dirty: Vec<Rect> = if full {
            vec![Rect {
                x: 0,
                y: 0,
                w: surface_w,
                h: surface_h,
            }]
        } else {
            let Some(rects) = sample.dirty_rects() else {
                // No attachment at all — not something we can work around, so
                // fall back to a full frame rather than painting nothing.
                if nth < FRAMES_TO_LOG {
                    info!("capture: frame {nth} has no dirtyRects attachment; repainting in full");
                }
                self.0.full_repaint.store(true, Ordering::Relaxed);
                return Ok(());
            };
            rects
                .iter()
                .filter_map(|r| clamp_rect(r, surface_w, surface_h))
                .collect()
        };
        if dirty.is_empty() {
            return Ok(());
        }

        // The cells this frame's damage touches, deduplicated: ScreenCaptureKit can
        // report overlapping rects, and snapping outward makes two nearby rects land
        // on the same cell often. Keyed by `(y, x)` so a frame's tiles arrive
        // top-down — the cells are disjoint, so this is for legibility rather than
        // correctness — and holding the whole `Rect` rather than re-deriving it from
        // the key keeps `split_cells` the only place that knows how an edge cell is
        // clipped.
        let cells: std::collections::BTreeMap<(u16, u16), Rect> = dirty
            .iter()
            .flat_map(|rect| split_cells(*rect, surface_w, surface_h))
            .map(|cell| ((cell.y, cell.x), cell))
            .collect();

        let mut tiles = Vec::new();
        let mut unchanged = 0usize;
        {
            let mut memo = self.0.cells.lock().unwrap();
            // A full repaint may be arriving at a canvas with nothing on it, so
            // nothing may be withheld from one.
            if full {
                memo.forget();
            }
            for cell in cells.into_values() {
                // Hashing the source rows first is the whole point: a cell
                // ScreenCaptureKit called dirty but did not change costs one read
                // of its pixels, not a pack plus a PNG.
                if memo.is_new(pixels, stride, cell) {
                    tiles.push(RawTile {
                        rect: cell,
                        rgb: extract_rgb(pixels, stride, cell),
                    });
                } else {
                    unchanged += 1;
                }
            }
        }
        if nth < FRAMES_TO_LOG {
            info!(
                "capture: frame {nth} status {status:?} — surface {surface_w}x{surface_h}, \
                 stride {stride}, full_repaint {full}, {} tile(s), {unchanged} unchanged",
                tiles.len()
            );
        }
        if !tiles.is_empty() {
            self.0.sink.tiles(tiles);
        }
        Ok(())
    }
}

/// Clamp a CoreGraphics dirty rect into the captured surface, dropping it
/// entirely if it falls outside or is degenerate.
///
/// The rects arrive as `f64` in surface pixels. They are trusted to be roughly
/// right but not to be exactly in bounds — rounding at the edges of a scaled
/// capture can put them a pixel over, and a read past the end of the surface is
/// not a rounding error, it is a crash.
fn clamp_rect(rect: &screencapturekit::cg::CGRect, surface_w: u16, surface_h: u16) -> Option<Rect> {
    // Round outward so a partially-covered pixel is repainted rather than left
    // stale: a one-pixel seam of old content is very visible.
    let x0 = rect.origin.x.floor().max(0.0);
    let y0 = rect.origin.y.floor().max(0.0);
    let x1 = (rect.origin.x + rect.size.width).ceil().min(f64::from(surface_w));
    let y1 = (rect.origin.y + rect.size.height).ceil().min(f64::from(surface_h));
    if !(x1 > x0 && y1 > y0) {
        return None;
    }
    Some(Rect {
        x: x0 as u16,
        y: y0 as u16,
        w: (x1 - x0) as u16,
        h: (y1 - y0) as u16,
    })
}

/// The grid cells covering `rect`, snapped outward and clipped to the surface.
///
/// Every pixel of `rect` is covered by exactly one cell, and cells never overlap,
/// so a frame's cells can be collected into a set without any question of ordering
/// or partial coverage. Cells at the right and bottom edges are clipped where the
/// surface does not divide by the cell size — unavoidable, and deterministic, which
/// is all a stable identity needs.
fn split_cells(rect: Rect, surface_w: u16, surface_h: u16) -> impl Iterator<Item = Rect> {
    let snap = |v: u16, step: u16| v - v % step;
    let right = rect.x.saturating_add(rect.w).min(surface_w);
    let bottom = rect.y.saturating_add(rect.h).min(surface_h);
    (snap(rect.y, CELL_H)..bottom)
        .step_by(usize::from(CELL_H))
        .flat_map(move |y| {
            (snap(rect.x, CELL_W)..right)
                .step_by(usize::from(CELL_W))
                .map(move |x| Rect {
                    x,
                    y,
                    w: CELL_W.min(surface_w - x),
                    h: CELL_H.min(surface_h - y),
                })
        })
}

/// Copy `rect` out of a BGRA surface as packed RGB888.
///
/// Reads row by row at `stride`, never `w * 4` — see the module docs. A row that
/// would run past the end of `pixels` is left black rather than panicking: a
/// short buffer is a bug elsewhere, and taking down the agent (which
/// `panic = "abort"` would do) is a worse outcome than one dark cell.
fn extract_rgb(pixels: &[u8], stride: usize, rect: Rect) -> Vec<u8> {
    let (w, h) = (usize::from(rect.w), usize::from(rect.h));
    let mut rgb = vec![0u8; w * h * 3];
    for row in 0..h {
        let src_start = (usize::from(rect.y) + row) * stride + usize::from(rect.x) * 4;
        let Some(src) = pixels.get(src_start..src_start + w * 4) else {
            debug!("capture: row {row} of {rect:?} is outside the surface; leaving it black");
            continue;
        };
        let dst = &mut rgb[row * w * 3..(row + 1) * w * 3];
        for (px, out) in src.chunks_exact(4).zip(dst.chunks_exact_mut(3)) {
            // BGRA -> RGB. The alpha channel is opaque for screen content and
            // the browser's canvas is opaque, so it is dropped.
            out[0] = px[2];
            out[1] = px[1];
            out[2] = px[0];
        }
    }
    rgb
}

fn clamp_u16(v: u32) -> u16 {
    v.min(u32::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    fn geometry_at(width: u16, height: u16, scale: f64) -> Geometry {
        Geometry {
            id: 1,
            width,
            height,
            scale,
            origin: (0.0, 0.0),
        }
    }

    // What a client puts under a display's name. Points, not captured pixels: a
    // 2x display listed at 3200×2400 reads as a resolution nobody chose, when
    // System Settings and the agent's own dialog both call it 1600×1200.
    #[test]
    fn the_detail_line_gives_the_size_in_points() {
        assert_eq!(detail(&geometry_at(3200, 2400, 2.0)), "1600×1200 at 2x");
        assert_eq!(detail(&geometry_at(1280, 800, 1.0)), "1280×800 at 1x");
        // A scale that is not a whole number stays readable rather than being
        // rounded away, and the points are still the honest division.
        assert_eq!(detail(&geometry_at(2560, 1600, 1.5)), "1707×1067 at 1.5x");
        // `geometry` falls back to 1.0 for a display whose mode it cannot read;
        // a zero must not divide the size to nothing here either.
        assert_eq!(detail(&geometry_at(1440, 900, 0.0)), "1440×900 at 1x");
    }

    // The created size and everything under it is HiDPI; the 1x modes macOS
    // offers past it are the ones that must not be captured at twice their
    // pixels. The exact boundary is the created size itself.
    #[test]
    fn an_owned_display_is_2x_up_to_the_size_it_was_created_at() {
        let base = (1600, 1000);
        assert_eq!(owned_scale((1600.0, 1000.0), base), crate::virtualdisplay::SCALE);
        assert_eq!(owned_scale((1344.0, 840.0), base), crate::virtualdisplay::SCALE);
        // A "(low resolution)" pick in System Settings: the whole panel's pixels
        // as points, which `maxPixels` proves cannot be backed at 2x.
        assert_eq!(owned_scale((3200.0, 2000.0), base), 1.0);
        // One axis over is enough — 1600x1200 would need 3200x2400 pixels.
        assert_eq!(owned_scale((1600.0, 1200.0), base), 1.0);
    }

    // The blind spot that made the reported density look random, pinned so the
    // fallback is never mistaken for a measurement again. macOS lists a 1x mode
    // beside the HiDPI one *at the same point size* — for a display created at
    // 1600x1000 the list holds both `1600x1000 pt / 3200x2000 px` and
    // `1600x1000 pt / 1600x1000 px` — and from points alone the two are one
    // number. Whichever macOS restored decided whether the answer below was
    // right. Only a pixel measurement can tell them apart, which is why
    // `owned_display_scale` starts with `framebuffer_scale` and this is the
    // fallback for when even that cannot be read.
    #[test]
    fn the_fallback_cannot_see_a_low_resolution_mode_at_the_created_size() {
        let base = (1600, 1000);
        // Genuinely 2x and genuinely 1x are the same point size to this, so this
        // one call is both of them: the 1x entry at the created size reads as 2x,
        // which is the reason this is a fallback and not the measurement.
        assert_eq!(
            owned_scale((1600.0, 1000.0), base),
            crate::virtualdisplay::SCALE
        );
    }

    // A client picks a display by the id this list reports, and a session
    // resolves that id with `probe`. If the two ever disagreed, a user would tick
    // one display and share another — so pin them against each other.
    #[test]
    fn every_listed_display_is_the_one_probe_resolves_by_id() {
        let Ok(displays) = displays(None) else {
            // No Screen Recording grant (or no window server at all), which is
            // the normal state for a `cargo test` run over SSH.
            eprintln!("cannot list displays in this session; skipping");
            return;
        };
        assert!(!displays.is_empty(), "the list is never empty on success");
        for display in &displays {
            let id = display.geometry.id;
            assert_eq!(probe(Target::Real(id)).unwrap(), display.geometry);
        }
        assert_eq!(
            displays.iter().filter(|display| display.is_main).count(),
            1,
            "exactly one display is the main one"
        );

        // An id that is not attached degrades to the main display rather than
        // failing, so a screen unplugged mid-session leaves a working one.
        let absent = displays
            .iter()
            .map(|display| display.geometry.id)
            .max()
            .unwrap_or(0)
            + 1;
        let main = displays
            .iter()
            .find(|display| display.is_main)
            .expect("one main display");
        assert_eq!(probe(Target::Real(absent)).unwrap(), main.geometry);
    }

    // Labels are what both clients put in a menu, so the numbering is part of the
    // contract rather than a detail of this function.
    #[test]
    fn labels_number_the_macs_own_screens_from_one() {
        let Ok(displays) = displays(None) else {
            eprintln!("cannot list displays in this session; skipping");
            return;
        };
        for (i, display) in displays.iter().enumerate() {
            assert_eq!(display.label, format!("Display {}", i + 1));
            assert!(!display.is_owned);
            assert!(
                display.summary().starts_with(&display.label),
                "the settings dialog's line leads with the label: {}",
                display.summary()
            );
        }

    }

    /// The property the grid exists for: every pixel of the damage rect is covered,
    /// exactly once, by a cell inside the surface.
    #[test]
    fn cells_cover_a_rect_exactly_once_and_stay_inside_the_surface() {
        // A surface indivisible by the cell size in both axes, so the right column
        // and bottom row are partial.
        let (sw, sh) = (1000u16, 150u16);
        let damage = rect(330, 70, 400, 60);
        let cells: Vec<Rect> = split_cells(damage, sw, sh).collect();

        let mut seen = std::collections::HashSet::new();
        for cell in &cells {
            for y in cell.y..cell.y + cell.h {
                for x in cell.x..cell.x + cell.w {
                    assert!(x < sw && y < sh, "{cell:?} leaves the surface");
                    assert!(seen.insert((x, y)), "({x},{y}) is covered twice");
                }
            }
        }
        for y in damage.y..damage.y + damage.h {
            for x in damage.x..damage.x + damage.w {
                assert!(seen.contains(&(x, y)), "({x},{y}) is not covered");
            }
        }
    }

    /// Outward, not clipped to the damage: this is what makes a cell's geometry the
    /// same every time, which is what both the memo below and the gateway's tile
    /// cache recognise repeats by.
    #[test]
    fn cells_snap_outward_rather_than_to_the_damage_rect() {
        // A cursor-sized rect in the middle of the second cell of the second row.
        let cells: Vec<Rect> = split_cells(rect(350, 80, 34, 15), 1600, 1000).collect();
        assert_eq!(cells, vec![rect(320, 64, 320, 64)]);
    }

    #[test]
    fn the_last_cell_of_a_row_is_clipped_to_the_surface() {
        let cells: Vec<Rect> = split_cells(rect(1500, 990, 100, 10), 1600, 1000).collect();
        assert_eq!(cells, vec![rect(1280, 960, 320, 40)]);
    }

    /// A full-width strip is the case that motivated the grid: 65% of the bytes
    /// measured on the test Mac were these, and only the cells that changed are
    /// worth sending.
    #[test]
    fn a_full_width_strip_becomes_one_row_of_cells() {
        let cells: Vec<Rect> = split_cells(rect(0, 128, 1600, 64), 1600, 1000).collect();
        assert_eq!(cells.len(), 5);
        assert!(cells.iter().all(|c| c.y == 128 && c.w == 320 && c.h == 64));
    }

    /// A `stride`-padded BGRA surface whose pixels are a function of `seed`.
    fn surface(w: usize, h: usize, stride: usize, seed: u8) -> Vec<u8> {
        let mut pixels = vec![0u8; stride * h];
        for row in 0..h {
            for col in 0..w {
                let at = row * stride + col * 4;
                let v = (row * w + col) as u8 ^ seed;
                pixels[at..at + 4].copy_from_slice(&[v, v.wrapping_add(1), v.wrapping_add(2), 255]);
            }
        }
        pixels
    }

    // The point of the memo: a cell ScreenCaptureKit called dirty but did not
    // change is answered without packing or encoding it.
    #[test]
    fn the_cell_memo_skips_unchanged_pixels_and_passes_changed_ones() {
        let (w, h, stride) = (8, 8, 40);
        let before = surface(w, h, stride, 0);
        let after = surface(w, h, stride, 0x5A);
        let cell = rect(0, 0, 8, 8);
        let mut memo = CellMemo::default();

        assert!(memo.is_new(&before, stride, cell), "never seen before");
        assert!(!memo.is_new(&before, stride, cell), "identical pixels");
        assert!(memo.is_new(&after, stride, cell), "the pixels changed");
        assert!(!memo.is_new(&after, stride, cell), "and now that one repeats");
    }

    // Padding bytes are not pixels. A digest that walked the buffer straight
    // through would fold them in and report a change whenever they moved.
    #[test]
    fn the_cell_digest_ignores_row_padding() {
        let (w, h) = (8, 4);
        let mut padded = surface(w, h, 40, 0);
        let tight = surface(w, h, w * 4, 0);
        let cell = rect(0, 0, 8, 4);
        assert_eq!(
            cell_digest(&padded, 40, cell),
            cell_digest(&tight, w * 4, cell),
            "the same pixels at two strides are the same picture"
        );
        // Scribble in the padding only: still the same picture.
        for row in 0..h {
            padded[row * 40 + w * 4] = 0xFF;
        }
        assert_eq!(
            cell_digest(&padded, 40, cell),
            cell_digest(&tight, w * 4, cell)
        );
    }

    // Two cells of different shapes must not match by hashing the same bytes.
    #[test]
    fn the_cell_digest_covers_the_cells_shape() {
        let pixels = surface(8, 8, 32, 0);
        assert_ne!(
            cell_digest(&pixels, 32, rect(0, 0, 8, 4)),
            cell_digest(&pixels, 32, rect(0, 0, 4, 8)),
            "same byte count, different shape"
        );
    }

    // A cell that runs past the surface is a bug elsewhere. The safe answer is to
    // send it, not to remember a digest of pixels that were never read.
    #[test]
    fn a_strip_outside_the_surface_is_never_remembered() {
        let pixels = surface(4, 4, 16, 0);
        let outside = rect(0, 8, 4, 4);
        assert_eq!(cell_digest(&pixels, 16, outside), None);
        let mut memo = CellMemo::default();
        assert!(memo.is_new(&pixels, 16, outside));
        assert!(memo.is_new(&pixels, 16, outside), "still not remembered");
    }

    // A full repaint may be arriving at a canvas with nothing on it.
    #[test]
    fn forgetting_makes_every_cell_paintable_again() {
        let pixels = surface(8, 8, 32, 0);
        let cell = rect(0, 0, 8, 8);
        let mut memo = CellMemo::default();
        assert!(memo.is_new(&pixels, 32, cell));
        assert!(!memo.is_new(&pixels, 32, cell));
        memo.forget();
        assert!(
            memo.is_new(&pixels, 32, cell),
            "a repaint must reach a blank canvas even with unchanged pixels"
        );
    }

    #[test]
    fn a_rect_smaller_than_a_cell_is_one_cell() {
        let cells: Vec<Rect> = split_cells(rect(0, 0, 8, 1), 1600, 1000).collect();
        assert_eq!(cells, vec![rect(0, 0, 320, 64)]);
        // Exactly one cell tall stays one row.
        assert_eq!(split_cells(rect(0, 0, 8, 64), 1600, 1000).count(), 1);
        // One pixel more crosses into the next.
        assert_eq!(split_cells(rect(0, 0, 8, 65), 1600, 1000).count(), 2);
    }

    /// A surface smaller than one cell still yields exactly one, sized to it —
    /// nothing here may read past the surface.
    #[test]
    fn a_surface_smaller_than_a_cell_yields_one_clipped_cell() {
        let cells: Vec<Rect> = split_cells(rect(0, 0, 40, 30), 40, 30).collect();
        assert_eq!(cells, vec![rect(0, 0, 40, 30)]);
    }

    // The stride bug this module exists to avoid: rows are `stride` apart, not
    // `width * 4`. With padding present, assuming width*4 shears the image.
    #[test]
    fn extract_reads_rows_at_the_reported_stride() {
        // 2x2 image with 4 bytes of row padding: stride 12, not 8.
        let stride = 12;
        let mut surface = vec![0u8; stride * 2];
        // Row 0: two pixels, BGRA.
        surface[0..4].copy_from_slice(&[1, 2, 3, 255]); // B=1 G=2 R=3
        surface[4..8].copy_from_slice(&[4, 5, 6, 255]);
        surface[8..12].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // padding
        // Row 1.
        surface[12..16].copy_from_slice(&[7, 8, 9, 255]);
        surface[16..20].copy_from_slice(&[10, 11, 12, 255]);
        surface[20..24].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // padding

        let rgb = extract_rgb(&surface, stride, rect(0, 0, 2, 2));
        // BGRA -> RGB, and no padding byte anywhere in the output.
        assert_eq!(rgb, vec![3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]);
    }

    #[test]
    fn extract_honours_the_rect_offset() {
        let stride = 16; // 4 pixels wide, no padding
        let mut surface = vec![0u8; stride * 3];
        for y in 0..3usize {
            for x in 0..4usize {
                let i = y * stride + x * 4;
                // Encode the coordinate in the pixel: B=x, G=y, R=0xF0.
                surface[i..i + 4].copy_from_slice(&[x as u8, y as u8, 0xF0, 255]);
            }
        }
        // The bottom-right 2x2.
        let rgb = extract_rgb(&surface, stride, rect(2, 1, 2, 2));
        assert_eq!(
            rgb,
            vec![
                0xF0, 1, 2, 0xF0, 1, 3, // row y=1, x=2..3
                0xF0, 2, 2, 0xF0, 2, 3, // row y=2
            ]
        );
    }

    // A read past the end must degrade to black, not abort the agent —
    // `panic = "abort"` in a dispatch-queue callback kills the process.
    #[test]
    fn extract_leaves_out_of_bounds_rows_black_instead_of_panicking() {
        let stride = 8;
        let surface = vec![0xFFu8; stride]; // one row only
        let rgb = extract_rgb(&surface, stride, rect(0, 0, 2, 3));
        assert_eq!(rgb.len(), 2 * 3 * 3);
        // Row 0 came through; rows 1 and 2 are black.
        assert_eq!(&rgb[..6], &[0xFF; 6]);
        assert_eq!(&rgb[6..], &[0u8; 12]);
    }

    fn cg_rect(x: f64, y: f64, w: f64, h: f64) -> screencapturekit::cg::CGRect {
        screencapturekit::cg::CGRect {
            origin: screencapturekit::cg::CGPoint { x, y },
            size: screencapturekit::cg::CGSize {
                width: w,
                height: h,
            },
        }
    }

    #[test]
    fn dirty_rects_are_clamped_into_the_surface() {
        // Wholly inside: unchanged.
        assert_eq!(
            clamp_rect(&cg_rect(10.0, 20.0, 30.0, 40.0), 100, 100),
            Some(rect(10, 20, 30, 40))
        );
        // Overhanging the right and bottom edges: trimmed, not dropped.
        assert_eq!(
            clamp_rect(&cg_rect(90.0, 90.0, 30.0, 30.0), 100, 100),
            Some(rect(90, 90, 10, 10))
        );
        // Negative origin: trimmed to 0.
        assert_eq!(
            clamp_rect(&cg_rect(-5.0, -5.0, 20.0, 20.0), 100, 100),
            Some(rect(0, 0, 15, 15))
        );
    }

    #[test]
    fn degenerate_and_offscreen_dirty_rects_are_dropped() {
        assert_eq!(clamp_rect(&cg_rect(0.0, 0.0, 0.0, 10.0), 100, 100), None);
        assert_eq!(clamp_rect(&cg_rect(0.0, 0.0, 10.0, 0.0), 100, 100), None);
        // Entirely past the right edge.
        assert_eq!(clamp_rect(&cg_rect(100.0, 0.0, 10.0, 10.0), 100, 100), None);
        assert_eq!(clamp_rect(&cg_rect(-20.0, 0.0, 10.0, 10.0), 100, 100), None);
    }

    // Fractional rects come out of a scaled capture; rounding inward would
    // leave a one-pixel seam of stale content, which is very visible.
    #[test]
    fn fractional_dirty_rects_round_outward() {
        assert_eq!(
            clamp_rect(&cg_rect(10.4, 20.6, 30.2, 40.1), 100, 100),
            Some(rect(10, 20, 31, 41))
        );
    }
}
