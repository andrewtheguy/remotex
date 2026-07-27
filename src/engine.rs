//! Helpers shared by every protocol engine.
//!
//! Deliberately *not* a `trait Engine`: the engines have very little in common
//! beyond their `run(config, input_rx, frame_tx)` signature — which is the seam
//! (see [`crate::session`]) — and IronRDP's non-`Send` futures could not
//! implement a trait object cleanly anyway. This module holds only the few
//! functions all three engines genuinely duplicate.
//!
//! It also owns the socket policy, which is not just formatting: [`tcp_connect`]
//! is the one place a remote host that has *gone away* is made noticeable. See
//! its comments for what the kernel can and cannot tell us.

use std::time::Duration;

use log::warn;
use tokio::net::TcpStream;

/// Idle time before the kernel starts probing a silent peer.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(10);
/// Gap between probes once they have started.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Unanswered probes before the socket fails.
const KEEPALIVE_RETRIES: u32 = 3;

/// Linux only, and the half that is easy to leave out.
///
/// Keepalive probes are sent on an *idle* connection: with unacknowledged data
/// outstanding, the retransmission timer owns the socket instead, and its budget
/// (`tcp_retries2`) runs to roughly fifteen minutes. That is exactly the state a
/// user puts the socket in by clicking at a desktop that has frozen — so
/// keepalive alone would cover the session nobody is touching and miss the one
/// somebody is. `TCP_USER_TIMEOUT` bounds any unacknowledged byte, and while it
/// is set it also bounds the keepalive failure, so one number covers both.
///
/// Set above [`keepalive_budget`] so the idle case is still reported as a
/// keepalive timeout. macOS has no equivalent option; a gateway running there
/// keeps the retransmission budget for a busy socket — see
/// `docs/known-issues.md`.
#[cfg(target_os = "linux")]
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the TCP connect itself may take.
///
/// A host that is switched off swallows SYNs, and the kernel's own retry budget
/// runs to about two minutes with the client showing "Connecting…" for all of
/// it — no client has a timeout of its own. Generous enough to cross a slow VPN.
/// [`crate::rxa`] keeps its own tighter budget, which covers connect *and*
/// handshake against an agent it expects on a LAN.
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a protocol handshake may take once the TCP connect has succeeded.
///
/// A host that accepts the connection and then says nothing is a hang no socket
/// timeout catches (see [`crate::rxa`], which has guarded this from the start).
/// Long enough for CredSSP or a DES challenge on a loaded server.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a silent host takes to be noticed: idle, then every probe.
///
/// Exposed so an error message quoting the number cannot drift from the
/// constants behind it.
pub fn keepalive_budget() -> Duration {
    KEEPALIVE_IDLE + KEEPALIVE_INTERVAL * KEEPALIVE_RETRIES
}

/// Connect to a remote, with the socket settings every engine wants.
///
/// `dest` arrives already formatted by [`host_port`] because each caller keeps it
/// for its own later error messages.
///
/// The keepalive is the point of this function. Without it a host that vanishes
/// without a FIN — powered off, or cut from the network — leaves the engine
/// blocked on a read forever, and the client holds a frozen desktop with nothing
/// to say. What it proves is narrow but real: that the peer's *kernel* is still
/// answering. For RDP and VNC that is the whole of it — a server process that
/// wedges behind a kernel which still answers reads as an idle desktop, and
/// neither RFB nor IronRDP offers a probe to close that gap. [`crate::rxa`] asks
/// the agent process as well, so for that engine this is the outer of two
/// guarantees rather than the only one (`docs/known-issues.md`).
pub async fn tcp_connect(dest: &str) -> anyhow::Result<TcpStream> {
    let stream = tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(dest))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "TCP connect to {dest}: no answer after {}s",
                TCP_CONNECT_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| anyhow::anyhow!("TCP connect to {dest}: {e}"))?;
    // Input events are tiny and latency-critical; never coalesce them.
    stream.set_nodelay(true).ok();
    if let Err(e) = arm_liveness_probes(&stream) {
        // Not a reason to refuse a session that otherwise works, but said out
        // loud rather than swallowed: this is a guarantee we no longer have.
        warn!("engine: could not arm TCP keepalive for {dest}: {e}");
    }
    Ok(stream)
}

/// Ask the kernel to notice a peer that has stopped answering.
fn arm_liveness_probes(stream: &TcpStream) -> std::io::Result<()> {
    let socket = socket2::SockRef::from(stream);
    // No `cfg` around these three: socket2 supports all of them on both targets
    // this gateway ships for (`with_time` is `TCP_KEEPALIVE` on macOS and
    // `TCP_KEEPIDLE` elsewhere). Whole seconds, because some platforms truncate.
    socket.set_tcp_keepalive(
        &socket2::TcpKeepalive::new()
            .with_time(KEEPALIVE_IDLE)
            .with_interval(KEEPALIVE_INTERVAL)
            .with_retries(KEEPALIVE_RETRIES),
    )?;
    #[cfg(target_os = "linux")]
    socket.set_tcp_user_timeout(Some(WRITE_TIMEOUT))?;
    Ok(())
}

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

    // The detection itself is not testable here, and the reason is the same one
    // that bounds what this feature can promise: any peer you can reach has a
    // kernel that answers keepalive probes, which is precisely the case these
    // options do *not* cover. What is testable — and what silently regresses if
    // the `all` feature or the call order changes — is that they reached the
    // socket at all.
    #[tokio::test]
    async fn tcp_connect_arms_the_liveness_probes_it_promises() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dest = listener.local_addr().unwrap().to_string();
        let accept = tokio::spawn(async move { listener.accept().await.unwrap() });

        let stream = tcp_connect(&dest).await.unwrap();
        let socket = socket2::SockRef::from(&stream);
        assert!(socket.keepalive().unwrap(), "SO_KEEPALIVE is off");
        assert_eq!(socket.tcp_keepalive_time().unwrap(), KEEPALIVE_IDLE);
        assert_eq!(socket.tcp_keepalive_interval().unwrap(), KEEPALIVE_INTERVAL);
        assert_eq!(socket.tcp_keepalive_retries().unwrap(), KEEPALIVE_RETRIES);
        #[cfg(target_os = "linux")]
        assert_eq!(socket.tcp_user_timeout().unwrap(), Some(WRITE_TIMEOUT));
        assert!(stream.nodelay().unwrap(), "input events must not coalesce");

        let _ = accept.await.unwrap();
    }

    #[test]
    fn the_keepalive_budget_is_the_sum_of_its_parts() {
        // The number an error message quotes to the user.
        assert_eq!(keepalive_budget(), Duration::from_secs(25));
        // A silent host must be reported well inside the browser's reattach
        // grace, or the session layer would expire the engine first and the user
        // would never see the reason.
        assert!(keepalive_budget() < crate::session::REATTACH_GRACE_PERIOD);
    }

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
