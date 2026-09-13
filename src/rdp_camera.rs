//! The browser's camera, into the RDP client's MS-RDPECAM device.
//!
//! The camera's counterpart of the sound adapter in [`crate::rdp`], and as small for the
//! same reason: the channel — the version, the announcement, the device's states, its one
//! media type and its metered samples — is [`rdpecam`], run on the RDP client's own
//! thread, and what is left here is the adapter between its two surfaces and
//! [`CameraBridge`]'s. The host's decisions become [`CameraSignal`]s; the camera socket's
//! format, samples and unplug become [`CameraFeed`] calls. Samples go straight through:
//! the browser encoded them, the Windows host decodes them, and nothing here looks inside
//! one.
//!
//! [`rdpecam`]: crate::rdp_client::proto::rdpecam

use std::sync::{Arc, Weak};

use crate::camera::{CAMERA_DEVICE_NAME, CameraBridge, CameraControl, CameraFormat, CameraSignal};
use crate::rdp_client::proto::rdpecam::Format;
use crate::rdp_client::{Camera, CameraFeed, CameraSink, Fed};

/// The `Connect::camera` for a target that carries a camera: the device's name, and a
/// sink that turns the host's decisions into the bridge's signals.
pub fn camera(bridge: Arc<CameraBridge>) -> Camera {
    Camera { name: CAMERA_DEVICE_NAME.to_owned(), sink: Box::new(Signals(bridge)) }
}

/// Make the session's camera feed the bridge's control, so the camera socket drives the
/// device.
///
/// The control holds the bridge weakly: the bridge holds the control, and a strong
/// reference back would keep both alive past the engine they belong to.
pub fn attach(bridge: &Arc<CameraBridge>, feed: CameraFeed) {
    bridge.set_control(Arc::new(Control { feed, bridge: Arc::downgrade(bridge) }));
}

/// The RDP client's sink, which is the bridge's signals with a shape on them. Every call
/// is an unbounded send, so the client's thread never waits here.
struct Signals(Arc<CameraBridge>);

impl CameraSink for Signals {
    // The negotiation and the device channel are said in the client's log; the browser
    // has nothing to do until an application on the host opens the camera.
    fn negotiated(&self, _version: u8) {}

    fn attached(&self) {}

    fn started(&self, format: Format) {
        self.0.signal(CameraSignal::Start(CameraFormat {
            width: format.width,
            height: format.height,
            fps_numerator: format.fps_numerator,
            fps_denominator: format.fps_denominator,
        }));
    }

    fn stopped(&self) {
        self.0.signal(CameraSignal::Stop);
    }

    fn keyframe_needed(&self) {
        self.0.signal(CameraSignal::Keyframe);
    }
}

/// The bridge's control, which is the session's camera feed with a shape on it.
struct Control {
    feed: CameraFeed,
    bridge: Weak<CameraBridge>,
}

impl CameraControl for Control {
    fn plug(&self, format: CameraFormat) {
        self.feed.plug(Format {
            width: format.width,
            height: format.height,
            fps_numerator: format.fps_numerator,
            fps_denominator: format.fps_denominator,
        });
    }

    fn unplug(&self) {
        self.feed.unplug();
    }

    fn sample(&self, data: &[u8], keyframe: bool) -> bool {
        match self.feed.sample(data, keyframe) {
            Fed::Queued => true,
            // The gap this opened is the browser's to close, with a keyframe.
            Fed::Dropped => {
                if let Some(bridge) = self.bridge.upgrade() {
                    bridge.signal(CameraSignal::Keyframe);
                }
                false
            }
            Fed::Skipped | Fed::Ended => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::error::TryRecvError;

    /// The host's decisions come out of the bridge as signals, with the format carried
    /// through field for field.
    #[test]
    fn the_hosts_decisions_become_signals() {
        let bridge = Arc::new(CameraBridge::new());
        let mut signals = bridge.subscribe();
        let Camera { name, sink } = camera(Arc::clone(&bridge));
        assert_eq!(name, CAMERA_DEVICE_NAME);

        sink.negotiated(2);
        sink.attached();
        sink.started(Format { width: 1280, height: 720, fps_numerator: 30_000, fps_denominator: 1_001 });
        sink.stopped();
        sink.keyframe_needed();

        assert_eq!(
            signals.try_recv(),
            Ok(CameraSignal::Start(CameraFormat {
                width: 1280,
                height: 720,
                fps_numerator: 30_000,
                fps_denominator: 1_001,
            }))
        );
        assert_eq!(signals.try_recv(), Ok(CameraSignal::Stop));
        assert_eq!(signals.try_recv(), Ok(CameraSignal::Keyframe));
        assert_eq!(signals.try_recv(), Err(TryRecvError::Empty));
    }
}
