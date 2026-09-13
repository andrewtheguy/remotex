//! The browser's camera, into FreeRDP's MS-RDPECAM endpoint.
//!
//! The mirror of [`crate::rdp_audio`], and just as small for the same reason:
//! the wrapper implements the channel — enumeration, announcement, media types,
//! credit metering — and what is left here is the adapter between its two
//! surfaces and [`CameraBridge`]'s two surfaces. Samples go straight through in
//! both name and fact: the browser encoded them, the Windows host decodes them,
//! and this file never looks inside one.
//!
//! ## The one rule
//!
//! **The event callbacks run on FreeRDP's threads** and must not block —
//! [`CameraBridge::signal`] is an unbounded send, which cannot. The control
//! direction has no such constraint: `plug`, `unplug` and `sample` are called
//! from the camera socket's task, and the wrapper queues or writes without
//! waiting on the session.

use std::sync::Arc;

use log::{debug, info};

use crate::camera::{CameraBridge, CameraControl, CameraFormat, CameraSignal, CAMERA_DEVICE_NAME};

/// The `Connect::camera` for a target, or `None` for one that asked for none.
///
/// The `Option` is already the answer, exactly as for sound:
/// [`crate::session`] builds a [`CameraBridge`] only for a target that opted
/// in, so a `None` here is a session that never registers `rdpecam` and never
/// offers the server a camera at all.
pub fn connect(bridge: Option<Arc<CameraBridge>>) -> Option<freerdp::Camera> {
    let bridge = bridge?;
    let camera = freerdp::Camera::new(
        CAMERA_DEVICE_NAME,
        Arc::new(Events { bridge: Arc::clone(&bridge) }),
    );
    bridge.set_control(Arc::new(Control { camera: camera.clone() }));
    Some(camera)
}

/// The wrapper's events, which are this gateway's signals with a shape on them.
struct Events {
    bridge: Arc<CameraBridge>,
}

impl freerdp::CameraEvents for Events {
    /// The host opened the enumeration channel: camera redirection is on offer.
    /// A log line rather than a signal — the browser has nothing to do with it,
    /// but "the host never negotiated" is the line that answers why a plugged
    /// camera never appeared, and policy can turn the channel off server-side.
    fn negotiated(&self, version: u8) {
        info!("camera: the host negotiated MS-RDPECAM version {version}");
    }

    /// The host connected the device channel: the virtual camera is installing.
    fn attached(&self) {
        info!("camera: the host attached the device channel");
    }

    fn started(&self, format: freerdp::CameraFormat) {
        self.bridge.signal(CameraSignal::Start(CameraFormat {
            width: format.width,
            height: format.height,
            fps_numerator: format.fps_numerator,
            fps_denominator: format.fps_denominator,
        }));
    }

    fn stopped(&self) {
        self.bridge.signal(CameraSignal::Stop);
    }

    fn keyframe_needed(&self) {
        debug!("camera: samples dropped; asking the browser for a keyframe");
        self.bridge.signal(CameraSignal::Keyframe);
    }
}

/// The bridge's control, which is the wrapper's camera with a shape on it.
struct Control {
    camera: freerdp::Camera,
}

impl CameraControl for Control {
    fn plug(&self, format: CameraFormat) {
        self.camera.plug(freerdp::CameraFormat {
            width: format.width,
            height: format.height,
            fps_numerator: format.fps_numerator,
            fps_denominator: format.fps_denominator,
        });
    }

    fn unplug(&self) {
        self.camera.unplug();
    }

    fn sample(&self, data: &[u8], keyframe: bool) -> bool {
        self.camera.sample(data, keyframe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::error::TryRecvError;

    /// A target that asked for no camera gets no channel, rather than a channel
    /// nothing feeds.
    #[test]
    fn no_bridge_means_no_rdpecam() {
        assert!(connect(None).is_none());
        assert!(connect(Some(Arc::new(CameraBridge::new()))).is_some());
    }

    /// The wrapper's decisions come out of the bridge as signals, with the
    /// format carried through field for field.
    #[test]
    fn the_hosts_decisions_become_signals() {
        use freerdp::CameraEvents as _;

        let bridge = Arc::new(CameraBridge::new());
        let mut signals = bridge.subscribe();
        let events = Events { bridge: Arc::clone(&bridge) };

        events.started(freerdp::CameraFormat {
            width: 1280,
            height: 720,
            fps_numerator: 30_000,
            fps_denominator: 1_001,
        });
        events.stopped();
        events.keyframe_needed();

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

    /// With no host streaming, a sample is refused rather than queued for ever:
    /// the wrapper's endpoint accepts samples only between start and stop.
    #[test]
    fn a_sample_before_the_host_starts_is_refused() {
        let bridge = Arc::new(CameraBridge::new());
        let camera = connect(Some(Arc::clone(&bridge)));
        assert!(camera.is_some());
        assert!(!bridge.sample(&[0, 0, 0, 1], true));
    }
}
