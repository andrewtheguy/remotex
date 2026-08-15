//! The seam between the one camera socket and an engine redirecting a webcam.
//!
//! The mirror image of [`crate::audio`], because the media flows the other way:
//! sound is the remote's and fans out to a listener, the camera is the
//! browser's and funnels in to the remote. What crosses this bridge inbound is
//! **already encoded** H.264 — the browser's own `VideoEncoder` made it, the
//! Windows host decodes it, and the gateway moves bytes without owning a codec,
//! the same bargain `audio_codec = "pcm"` strikes in the other direction.
//! Outbound go the host's streaming decisions — start, stop, "next one must be
//! a keyframe" — which are the only things a capturing browser cannot know.
//!
//! Nothing here names an engine. The engine side registers a [`CameraControl`]
//! and publishes [`CameraSignal`]s; RDP's adapter is [`crate::rdp_camera`], and
//! it is the only implementor, because MS-RDPECAM is the one camera channel any
//! of the gateway's protocols has.

use std::sync::{Arc, Mutex};

use log::debug;
use tokio::sync::mpsc;

/// The name Windows shows beside the redirected device.
pub const CAMERA_DEVICE_NAME: &str = "Remotex Camera";

/// The geometry and rate of the H.264 the browser will send, as announced on the
/// camera socket's first message and advertised to the remote verbatim.
///
/// A rational rate rather than an integer because both ends speak one: browsers
/// report track rates like 29.97, and MS-RDPECAM's media type carries a
/// numerator and denominator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraFormat {
    pub width: u32,
    pub height: u32,
    pub fps_numerator: u32,
    pub fps_denominator: u32,
}

/// A streaming decision of the remote's, on its way to the camera socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraSignal {
    /// The remote started the stream — an application on it opened the camera —
    /// and samples will be consumed from now on. The format is the remote's
    /// confirmation, and it is the one the socket plugged with: the gateway
    /// advertises exactly one media type, so nothing else can be selected.
    Start(CameraFormat),
    /// The remote stopped the stream. Encoding can stop; the device stays
    /// plugged, and another [`CameraSignal::Start`] may follow.
    Stop,
    /// Samples were dropped and the stream cannot continue mid-GOP: the next
    /// sample must be a keyframe, and everything before one is discarded.
    Keyframe,
}

/// The engine half a camera socket drives: plug the device, feed it, unplug it.
///
/// Every method may be called from any thread and must not block — `sample`
/// runs once per encoded frame on the socket task.
pub trait CameraControl: Send + Sync {
    /// Announce the device to the remote as a camera producing H.264 in `format`.
    fn plug(&self, format: CameraFormat);
    /// Withdraw the device; the remote sees the camera unplug.
    fn unplug(&self);
    /// One encoded H.264 access unit. Returns whether it was accepted — a
    /// refusal means the sample was dropped and a [`CameraSignal::Keyframe`]
    /// is on its way.
    fn sample(&self, data: &[u8], keyframe: bool) -> bool;
}

/// The seam itself: one per engine that carries a camera, created by
/// [`crate::session`] alongside the audio bridge and handed to both halves.
///
/// Unlike the audio bridge there is no queue in it — samples go straight
/// through to the engine's control, which does its own credit metering — so
/// what the bridge holds is just the two registrations: the engine's control
/// and the socket's signal sender, either of which can come and go while the
/// other stays.
#[derive(Default)]
pub struct CameraBridge {
    control: Mutex<Option<Arc<dyn CameraControl>>>,
    signals: Mutex<Option<mpsc::UnboundedSender<CameraSignal>>>,
}

impl std::fmt::Debug for CameraBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraBridge").finish_non_exhaustive()
    }
}

impl CameraBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// The engine registers its control as it starts. There is exactly one
    /// engine per bridge, so a second registration is a replaced engine —
    /// which cannot happen today, and would be harmless if it did.
    pub fn set_control(&self, control: Arc<dyn CameraControl>) {
        *self.control.lock().expect("camera control lock") = Some(control);
    }

    /// The socket subscribes for the remote's decisions, replacing any earlier
    /// subscriber: signals go to the one live camera socket, and a superseded
    /// socket's receiver simply ends.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<CameraSignal> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.signals.lock().expect("camera signal lock") = Some(tx);
        rx
    }

    /// Publish one of the remote's decisions. Called on an engine thread and
    /// never blocks: the channel is unbounded and these are rare — a start, a
    /// stop, the occasional keyframe request — not per-frame traffic.
    pub fn signal(&self, signal: CameraSignal) {
        let signals = self.signals.lock().expect("camera signal lock");
        if let Some(tx) = signals.as_ref()
            && tx.send(signal).is_err()
        {
            debug!("camera: a signal arrived with no socket to hear it: {signal:?}");
        }
    }

    /// Plug the device, if an engine is listening.
    pub fn plug(&self, format: CameraFormat) {
        if let Some(control) = self.control() {
            control.plug(format);
        }
    }

    /// Unplug the device, if an engine is listening.
    pub fn unplug(&self) {
        if let Some(control) = self.control() {
            control.unplug();
        }
    }

    /// Hand one encoded sample to the engine, if one is listening.
    pub fn sample(&self, data: &[u8], keyframe: bool) -> bool {
        match self.control() {
            Some(control) => control.sample(data, keyframe),
            None => false,
        }
    }

    fn control(&self) -> Option<Arc<dyn CameraControl>> {
        self.control.lock().expect("camera control lock").clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Recorder {
        plugs: AtomicUsize,
        unplugs: AtomicUsize,
        samples: AtomicUsize,
    }

    impl CameraControl for Recorder {
        fn plug(&self, _format: CameraFormat) {
            self.plugs.fetch_add(1, Ordering::Relaxed);
        }
        fn unplug(&self) {
            self.unplugs.fetch_add(1, Ordering::Relaxed);
        }
        fn sample(&self, _data: &[u8], _keyframe: bool) -> bool {
            self.samples.fetch_add(1, Ordering::Relaxed);
            true
        }
    }

    const FORMAT: CameraFormat =
        CameraFormat { width: 640, height: 480, fps_numerator: 30, fps_denominator: 1 };

    /// A bridge with no engine refuses samples rather than losing them silently
    /// mid-claim: the socket sees the refusal and can tell the browser.
    #[test]
    fn a_bridge_with_no_engine_accepts_nothing() {
        let bridge = CameraBridge::new();
        bridge.plug(FORMAT);
        bridge.unplug();
        assert!(!bridge.sample(&[1, 2, 3], true));
    }

    #[test]
    fn the_engines_control_receives_the_sockets_traffic() {
        let bridge = CameraBridge::new();
        let recorder = Arc::new(Recorder::default());
        bridge.set_control(recorder.clone());
        bridge.plug(FORMAT);
        assert!(bridge.sample(&[1], true));
        bridge.unplug();
        assert_eq!(recorder.plugs.load(Ordering::Relaxed), 1);
        assert_eq!(recorder.samples.load(Ordering::Relaxed), 1);
        assert_eq!(recorder.unplugs.load(Ordering::Relaxed), 1);
    }

    /// A second subscriber supersedes the first, the same way a second audio
    /// socket supersedes the first: signals follow the live socket.
    #[tokio::test]
    async fn a_second_subscriber_supersedes_the_first() {
        let bridge = CameraBridge::new();
        let mut first = bridge.subscribe();
        let mut second = bridge.subscribe();
        bridge.signal(CameraSignal::Start(FORMAT));
        assert_eq!(second.recv().await, Some(CameraSignal::Start(FORMAT)));
        assert_eq!(first.recv().await, None);
    }

    /// Signals with no subscriber are dropped rather than queued: a socket that
    /// attaches later must not replay a stale start.
    #[tokio::test]
    async fn signals_do_not_wait_for_a_subscriber() {
        let bridge = CameraBridge::new();
        bridge.signal(CameraSignal::Start(FORMAT));
        let mut rx = bridge.subscribe();
        bridge.signal(CameraSignal::Stop);
        assert_eq!(rx.recv().await, Some(CameraSignal::Stop));
    }
}
