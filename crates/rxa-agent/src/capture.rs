//! ScreenCaptureKit capture: native dirty rects in, packed-RGB tiles out.
//!
//! ## Why dirty rects are load-bearing
//!
//! ScreenCaptureKit hands us the regions that actually changed, in the
//! `SCStreamFrameInfo.dirtyRects` attachment on each `CMSampleBuffer`. Encoding
//! only those is the difference between a usable Retina session and one that
//! pegs a core diffing full frames. This is the single most important API
//! detail in the whole agent, and it is why `screencapturekit` was chosen over
//! `objc2-screen-capture-kit`: it exposes the attachment directly.
//!
//! ## The seam
//!
//! Everything above this module deals only in [`RawTile`] — a rectangle plus
//! packed RGB888 — via the [`FrameSink`] trait. Swapping the binding crate, or
//! moving to full-frame diffing if a future macOS stops reporting dirty rects,
//! is contained here.
//!
//! ## Two bugs this module exists to not have
//!
//! - **Stride.** `CVPixelBuffer::bytes_per_row` is *not* `width * 4`; macOS
//!   pads rows for alignment. Every read goes row by row at the reported
//!   stride. Assuming `width * 4` produces a picture that shears progressively
//!   further right as it goes down — the classic ScreenCaptureKit bug.
//! - **Backing scale.** A Retina display is captured at pixel dimensions that
//!   differ from the point dimensions `CGEventPost` wants. We capture at full
//!   pixel size for fidelity and report the measured scale so
//!   [`crate::input`] can divide by it. The scale is *measured* from the
//!   surface rather than assumed, because assuming it is how clicks end up in
//!   the wrong place.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use log::{debug, info, warn};
use screencapturekit::stream::delegate_trait::ErrorHandler;
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;

/// Dirty rectangles taller than this are split into strips, so a full-screen
/// repaint becomes many small messages the browser can start painting
/// immediately rather than one enormous one. Mirrors the gateway's
/// `protocol::STRIP_ROWS`, for the same reason.
const STRIP_ROWS: u16 = 64;

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

/// Which display a session shares.
///
/// Both arms name a `CGDirectDisplayID`, which is also what a client selects by
/// ([`rxa_proto::msg::DisplayEntry::id`]). Position in the shareable list is
/// deliberately not an identity: attaching or unplugging a screen renumbers
/// every display after it, so a target held across that would quietly become a
/// different screen.
///
/// Two kinds all the same, because a display of our own needs its creation size
/// carried alongside its id: that size is the fallback for reading its backing
/// scale (see [`owned_display_scale`]) and the ceiling `maxPixels` fixed at
/// creation. Its mode *is* readable, contrary to what this said before.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    /// One of the Mac's own displays, by CoreGraphics display id.
    Real(u32),
    /// A display we created, by id, plus the point size it was created at.
    ///
    /// The display shows up in System Settings like any other, so whoever is
    /// using the Mac can put it in any mode on the list macOS derived —
    /// including the `(low resolution)` 1x entries. The size is what
    /// [`owned_scale`] falls back to when the mode cannot be read.
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

/// The backing scale of a display of our own.
///
/// Read from the CoreGraphics mode, exactly as a real display's is. These
/// displays *do* publish one — measured on macOS 26.5.2 in the test VM, where
/// `CGDisplayCopyDisplayMode` returns `3800x2400 px / 1900x1200 pt` for a 2x
/// display and `1900x1200 px / 1900x1200 pt` for the same display switched to
/// 1x. Earlier notes here said it returned NULL for them and built a heuristic
/// around that; it does not, and the heuristic was wrong in a way nothing else
/// could catch (see [`owned_scale`]).
///
/// [`owned_scale`] remains the fallback for a macOS where the mode really is
/// absent, since nothing promises a private display keeps publishing one.
fn owned_display_scale(id: u32, points: (f64, f64), base: (u32, u32)) -> f64 {
    mode_scale(id).unwrap_or_else(|| owned_scale(points, base))
}

/// Fallback scale for a display of our own, from the size it was created at.
///
/// `maxPixels` was set to [`crate::virtualdisplay::SCALE`] times the created
/// size and cannot be changed, so that size is exactly the largest mode macOS
/// can put twice the pixels behind: at or under it, assume it did; over it, it
/// provably did not and the mode is 1x.
///
/// **It cannot see the case that matters.** macOS lists a `(low resolution)` 1x
/// entry beside each HiDPI one *at the same point size* — the mode list for a
/// 1900x1200 display holds both `1900x1200 pt / 3800x2400 px` and
/// `1900x1200 pt / 1900x1200 px` — and both are "at or under the created size",
/// so this answers 2x for either. Whichever one macOS restored for the identity
/// then decided whether the agent was right, which is what made the reported
/// density look random from the outside. Only [`owned_display_scale`]'s mode
/// read separates them; this is the degraded answer when there is no mode.
///
/// Getting this wrong is expensive in one direction — reading a 3200x2000 1x
/// mode as 2x would ask ScreenCaptureKit for a 6400x4000 surface and hand the
/// encoder four times the pixels for an upscale of the same desktop.
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
    /// `"Display 2 · 1440×900 at 1x"`, for the agent's own read-only list.
    pub fn summary(&self) -> String {
        format!("{} · {}", self.label, self.detail)
    }
}

/// The displays the agent could share.
///
/// `owned` is the display the agent created, if it created one; it is in this
/// list like any other display, but only the caller knows which id is ours and
/// [`owned_scale`] is the only way to measure it correctly.
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

    /// Re-measure the captured display and resize the stream's surface to match.
    ///
    /// A running stream's surface size is **fixed at the size it was configured
    /// with**. When the display then changes mode, ScreenCaptureKit does not
    /// resize the surface — it scales the new desktop into the old one. So the
    /// frames keep arriving at the old dimensions, the handler never sees a size
    /// change, and nothing tells the browser anything happened; what it shows is
    /// a squashed picture of the new resolution. That holds however the mode
    /// changed: a mode switch made on the Mac itself, or the
    /// host resizing a VM's virtual display.
    ///
    /// This does not notify anyone. It resizes the surface, and the next frame
    /// then arrives at the new size, which is what makes the handler's existing
    /// resize path fire — one announcement path for every cause.
    ///
    /// Returns the geometry now being captured, or `None` when nothing was done
    /// — either the display has not in fact changed, or it is mid-reconfigure
    /// and momentarily has no mode to read. The second is not an error: it is
    /// the normal state for a few polls around a resize, and reporting it as one
    /// would log a warning every 100ms through exactly the event this exists to
    /// handle. The poll after it sees the new mode.
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
/// The owned case still does not go through [`geometry_for_id`], but not for the
/// reason once written here. That path returns `None` when a display has no mode,
/// meaning "mid-reconfigure, try again"; for a display of our own the absence of a
/// mode is a state to degrade in, not to stall in, since bounds *are* always
/// published for these displays and [`owned_display_scale`] has a fallback for the
/// scale. So bounds drive the size and the mode is consulted only for the scale.
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
        // Read like a real display's, with the created size only as a fallback:
        // an owned display does publish a mode, and it is the one reading that
        // tells a HiDPI mode from the `(low resolution)` 1x entry at the same
        // point size.
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
/// bounds are the one reading always published for those. Points is what
/// [`crate::virtualdisplay::VirtualDisplay::set_scale`] needs: it re-lists the
/// same logical size at a different density, so the desktop keeps its layout.
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
/// The distinction matters for a display of our own: "no mode" is the case
/// [`owned_display_scale`] has to fall back for, and it is not the same answer
/// as "1x".
fn mode_scale(id: u32) -> Option<f64> {
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

        let mut tiles = Vec::new();
        for rect in dirty {
            for strip in split_strips(rect) {
                tiles.push(RawTile {
                    rect: strip,
                    rgb: extract_rgb(pixels, stride, strip),
                });
            }
        }
        if nth < FRAMES_TO_LOG {
            info!(
                "capture: frame {nth} status {status:?} — surface {surface_w}x{surface_h}, \
                 stride {stride}, full_repaint {full}, {} tile(s)",
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

/// Split a rectangle into strips at most [`STRIP_ROWS`] tall.
fn split_strips(rect: Rect) -> impl Iterator<Item = Rect> {
    (0..rect.h)
        .step_by(usize::from(STRIP_ROWS))
        .map(move |dy| Rect {
            x: rect.x,
            y: rect.y + dy,
            w: rect.w,
            h: STRIP_ROWS.min(rect.h - dy),
        })
}

/// Copy `rect` out of a BGRA surface as packed RGB888.
///
/// Reads row by row at `stride`, never `w * 4` — see the module docs. A row that
/// would run past the end of `pixels` is left black rather than panicking: a
/// short buffer is a bug elsewhere, and taking down the agent (which
/// `panic = "abort"` would do) is a worse outcome than one dark strip.
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
    // right. Only `owned_display_scale`'s mode read can tell them apart, which is
    // why it is what both call sites use and this is only the no-mode fallback.
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

    #[test]
    fn strips_tile_a_tall_rect_without_gaps_or_overlap() {
        let strips: Vec<Rect> = split_strips(rect(10, 20, 100, 150)).collect();
        assert_eq!(strips.len(), 3);
        assert_eq!(strips[0], rect(10, 20, 100, 64));
        assert_eq!(strips[1], rect(10, 84, 100, 64));
        // The last strip is short, not padded past the rect.
        assert_eq!(strips[2], rect(10, 148, 100, 22));
        // Contiguous: each strip starts where the previous ended.
        for pair in strips.windows(2) {
            assert_eq!(pair[1].y, pair[0].y + pair[0].h);
        }
        assert_eq!(
            strips.iter().map(|s| u32::from(s.h)).sum::<u32>(),
            150,
            "the strips must cover the rect exactly"
        );
    }

    #[test]
    fn a_short_rect_is_one_strip() {
        let strips: Vec<Rect> = split_strips(rect(0, 0, 8, 1)).collect();
        assert_eq!(strips, vec![rect(0, 0, 8, 1)]);
        // Exactly one strip tall stays one strip.
        let strips: Vec<Rect> = split_strips(rect(0, 0, 8, 64)).collect();
        assert_eq!(strips, vec![rect(0, 0, 8, 64)]);
        // One pixel more is two.
        assert_eq!(split_strips(rect(0, 0, 8, 65)).count(), 2);
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
