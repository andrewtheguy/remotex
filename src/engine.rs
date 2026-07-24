//! Helpers shared by every protocol engine.
//!
//! Deliberately *not* a `trait Engine`: the engines have very little in common
//! beyond their `run(config, input_rx, frame_tx)` signature — which is the seam
//! (see [`crate::session`]) — and IronRDP's non-`Send` futures could not
//! implement a trait object cleanly anyway. This module holds only the few
//! functions all three engines genuinely duplicate.

/// Format a `host:port` destination for `TcpStream::connect`, bracketing bare
/// IPv6 literals (e.g. `fdb8::20` -> `[fdb8::20]:3389`).
pub fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Clamp a browser pointer coordinate into the protocol's `u16` range.
///
/// [`crate::protocol::ClientMsg::MouseMove`] carries `i32` because that is what
/// the DOM produces, and a drag off the canvas edge legitimately reports
/// negative or oversized values. Clamping — rather than dropping the event —
/// keeps a drag that leaves the canvas pinned to the edge instead of freezing.
pub fn clamp_u16(v: i32) -> u16 {
    v.clamp(0, i32::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_brackets_bare_ipv6_literals_only() {
        assert_eq!(host_port("10.0.0.2", 3389), "10.0.0.2:3389");
        assert_eq!(host_port("mac.local", 52381), "mac.local:52381");
        assert_eq!(host_port("fdb8::20", 5900), "[fdb8::20]:5900");
        assert_eq!(
            host_port("fdb8:d92a:f690:3d7f:97a4:120a:2:20", 3389),
            "[fdb8:d92a:f690:3d7f:97a4:120a:2:20]:3389"
        );
        // An address the user already bracketed is left alone.
        assert_eq!(host_port("[fdb8::20]", 5900), "[fdb8::20]:5900");
    }

    #[test]
    fn clamp_u16_pins_out_of_range_coordinates_to_the_edge() {
        assert_eq!(clamp_u16(0), 0);
        assert_eq!(clamp_u16(1279), 1279);
        assert_eq!(clamp_u16(-1), 0);
        assert_eq!(clamp_u16(i32::MIN), 0);
        assert_eq!(clamp_u16(65535), u16::MAX);
        assert_eq!(clamp_u16(70000), u16::MAX);
        assert_eq!(clamp_u16(i32::MAX), u16::MAX);
    }
}
