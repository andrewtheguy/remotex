//! Display Control: how a client asks for a different desktop size.
//!
//! One dynamic channel ([`super::dvc`]), one PDU each way. The server opens the
//! channel by [`CHANNEL_NAME`] and sends its [`Capabilities`] — how many monitors it
//! will lay out and how large they may be together. After that the client may send a
//! **monitor layout** whenever the window it is presenting the desktop in changes
//! size, and the server answers by tearing the share down and building it again at
//! the new size: a Deactivate All, then a fresh capability exchange.
//!
//! # One monitor, upright, of unknown physical size
//!
//! This gateway presents one desktop to one browser, so it sends one monitor and
//! marks it primary. Three fields that a client with a real display would fill in are
//! deliberately left at zero:
//!
//! - **Orientation.** A window taller than it is wide is not a rotated monitor. Every
//!   desktop client sends 0 whatever its shape, and a server that is told 90 turns the
//!   desktop on its side — which is what a phone held upright would ask for.
//! - **Physical width and height.** MS-RDPEDISP's "unknown", and the honest answer
//!   from a client with no display of its own.
//! - **Scale**, unless the caller names one in range. The desktop and device scale
//!   factors are read as a pair, and a server that finds either invalid ignores both,
//!   so an out-of-range one is written as no scale at all rather than as half a pair.
//!
//! [MS-RDPEDISP] 2.2.

use super::wire::{Malformed, Reader, Writer};

/// The name the server opens the channel under, which is how it is told from every
/// other dynamic channel a Windows host offers.
pub const CHANNEL_NAME: &str = "Microsoft::Windows::RDS::DisplayControl";

/// The smallest and largest desktop a monitor layout may ask for, in either
/// direction. A width must also be even.
pub const MIN_DIMENSION: u32 = 200;
pub const MAX_DIMENSION: u32 = 8192;

/// The range a desktop scale factor is read in. Outside it, the server ignores the
/// scale — see the module doc.
pub const MIN_SCALE: u32 = 100;
pub const MAX_SCALE: u32 = 500;

/// The device scale factor, which has three permitted values and is pinned to the
/// first: this client has no display of its own to have been built for.
const DEVICE_SCALE: u32 = 100;

/// `DISPLAYCONTROL_HEADER` types.
const CAPABILITIES: u32 = 0x0000_0005;
const MONITOR_LAYOUT: u32 = 0x0000_0002;

/// The type and length every PDU on this channel starts with.
const HEADER: u32 = 8;

/// `DISPLAYCONTROL_MONITOR_PRIMARY`.
const PRIMARY: u32 = 0x0000_0001;

/// One `DISPLAYCONTROL_MONITOR_LAYOUT`, whose size the layout PDU announces.
const ENTRY: u32 = 40;

/// A whole monitor layout PDU: the header, the monitor count, and one entry.
const LAYOUT: u32 = HEADER + 8 + ENTRY;

const WHAT: &str = "a Display Control PDU";

/// What the server will lay out, out of its capabilities PDU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// How many monitors a layout may name. This client sends one.
    pub monitors: u32,
    /// The largest total area, in pixels, that a layout may come to — the product of
    /// the two factors the server names and the number of monitors it allows.
    pub area: u64,
}

/// Read the capabilities PDU that opens the channel.
///
/// A monitor layout PDU on this channel would be this client's own, echoed, and is
/// refused rather than read: nothing above has anything to do with one.
pub fn capabilities(payload: &[u8]) -> Result<Capabilities, Malformed> {
    let mut r = Reader::new(WHAT, payload);
    let kind = r.u32_le()?;
    if kind != CAPABILITIES {
        return Err(r.refuse("a PDU type", kind));
    }
    let length = r.u32_le()?;
    if usize::try_from(length).unwrap_or(usize::MAX) != payload.len() {
        return Err(r.refuse("a length that is not the PDU's", length));
    }
    let monitors = r.u32_le()?;
    let a = u64::from(r.u32_le()?);
    let b = u64::from(r.u32_le()?);
    Ok(Capabilities { monitors, area: a * b * u64::from(monitors) })
}

/// Bring a desktop size inside what a monitor layout may ask for: [`MIN_DIMENSION`]
/// to [`MAX_DIMENSION`], with an odd width rounded *down* so a desktop stays inside
/// the window that asked for it rather than growing a scrollbar by one pixel.
///
/// Public because a caller deciding *whether to ask at all* has to compare against
/// the size that would really be sent: asking a host for the desktop it already has
/// is not free — it answers with a full resize — so a client comparing an unadjusted
/// 1281 against a live 1280 would ask again forever.
pub fn adjust_size(width: u32, height: u32) -> (u32, u32) {
    (width.clamp(MIN_DIMENSION, MAX_DIMENSION) & !1, height.clamp(MIN_DIMENSION, MAX_DIMENSION))
}

/// Ask the host for a desktop of this size.
///
/// `width` and `height` come from a browser window and are clamped to what the
/// protocol permits — [`MIN_DIMENSION`] to [`MAX_DIMENSION`], with an odd width
/// rounded down — rather than refused: a window can be any size, and the nearest
/// desktop the host will open is a better answer than none. `scale` is the browser's
/// pixel density as a percentage, and is written only if the server would read it.
pub fn monitor_layout(width: u32, height: u32, scale: u32) -> Vec<u8> {
    let (width, height) = adjust_size(width, height);
    let scale = if (MIN_SCALE..=MAX_SCALE).contains(&scale) { Some(scale) } else { None };

    let mut w = Writer::with_capacity(LAYOUT as usize);
    w.u32_le(MONITOR_LAYOUT);
    w.u32_le(LAYOUT);
    w.u32_le(ENTRY); // MonitorLayoutSize, the size of one entry
    w.u32_le(1); // NumMonitors
    w.u32_le(PRIMARY);
    w.u32_le(0); // Left
    w.u32_le(0); // Top
    w.u32_le(width);
    w.u32_le(height);
    w.u32_le(0); // PhysicalWidth
    w.u32_le(0); // PhysicalHeight
    w.u32_le(0); // Orientation
    w.u32_le(scale.unwrap_or(0)); // DesktopScaleFactor
    w.u32_le(scale.map_or(0, |_| DEVICE_SCALE));
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capabilities PDU as a server sends one.
    fn caps(monitors: u32, a: u32, b: u32) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32_le(CAPABILITIES);
        w.u32_le(20);
        w.u32_le(monitors);
        w.u32_le(a);
        w.u32_le(b);
        w.finish()
    }

    #[test]
    fn the_capabilities_say_how_large_a_desktop_may_be_asked_for() {
        assert_eq!(
            capabilities(&caps(16, 8192, 8192)).unwrap(),
            Capabilities { monitors: 16, area: 8192 * 8192 * 16 }
        );
    }

    #[test]
    fn a_pdu_that_is_not_the_capabilities_is_refused_by_type() {
        let layout = monitor_layout(1280, 800, 100);
        let err = capabilities(&layout).unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "a PDU type", value: 2, .. }));

        let mut truncated = caps(1, 8192, 8192);
        truncated.pop();
        let err = capabilities(&truncated).unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "a length that is not the PDU's", .. }));
    }

    /// The whole PDU, byte for byte: it never varies in shape, only in the four
    /// numbers a caller sets.
    #[test]
    fn a_layout_is_one_primary_monitor_of_the_size_that_was_asked_for() {
        let bytes = monitor_layout(1280, 800, 150);
        let mut r = Reader::new("a test", &bytes);
        assert_eq!(r.u32_le().unwrap(), MONITOR_LAYOUT);
        assert_eq!(r.u32_le().unwrap(), LAYOUT);
        assert_eq!(r.u32_le().unwrap(), ENTRY);
        assert_eq!(r.u32_le().unwrap(), 1);
        assert_eq!(r.u32_le().unwrap(), PRIMARY);
        assert_eq!(r.u32_le().unwrap(), 0); // Left
        assert_eq!(r.u32_le().unwrap(), 0); // Top
        assert_eq!(r.u32_le().unwrap(), 1280);
        assert_eq!(r.u32_le().unwrap(), 800);
        assert_eq!(r.u32_le().unwrap(), 0); // PhysicalWidth
        assert_eq!(r.u32_le().unwrap(), 0); // PhysicalHeight
        assert_eq!(r.u32_le().unwrap(), 0); // Orientation, never rotated
        assert_eq!(r.u32_le().unwrap(), 150);
        assert_eq!(r.u32_le().unwrap(), DEVICE_SCALE);
        assert!(r.is_empty());
        assert_eq!(bytes.len(), LAYOUT as usize);
    }

    /// A window is whatever size the person made it; a desktop has to be one the
    /// protocol allows.
    #[test]
    fn a_size_a_host_would_refuse_is_brought_into_range_rather_than_sent() {
        let sizes =
            [((1367, 768), (1366, 768)), ((100, 99), (200, 200)), ((9000, 9000), (8192, 8192))];
        for (asked, sent) in sizes {
            let bytes = monitor_layout(asked.0, asked.1, 100);
            let mut r = Reader::new("a test", &bytes[28..]);
            assert_eq!((r.u32_le().unwrap(), r.u32_le().unwrap()), sent, "asked for {asked:?}");
        }
    }

    /// The two scale factors are read as a pair, so one out of range means neither is
    /// sent.
    #[test]
    fn a_scale_the_server_would_ignore_is_left_out_with_the_one_beside_it() {
        for scale in [0, 99, 501] {
            let bytes = monitor_layout(1280, 800, scale);
            assert_eq!(&bytes[48..], &[0; 8], "a scale of {scale}");
        }
        let bytes = monitor_layout(1280, 800, 500);
        assert_eq!(&bytes[48..], &[0xF4, 0x01, 0, 0, 100, 0, 0, 0]);
    }
}
