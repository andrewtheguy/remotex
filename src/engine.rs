//! Helpers shared by every protocol engine.
//!
//! Deliberately *not* a `trait Engine`: the engines have very little in common
//! beyond their `run(config, input_rx, frame_tx)` signature — which is the seam
//! (see [`crate::session`]). This module holds only the few things they genuinely
//! share.
//!
//! It also owns the socket policy, which is not just formatting: it is where a
//! remote host that has *gone away* is made noticeable. See [`tcp_connect`]'s
//! comments for what the kernel can and cannot tell us.
//!
//! **Only VNC opens its own socket now.** The RDP engine hands these same numbers
//! to FreeRDP, which applies them itself in `libfreerdp/core/tcp.c` — see
//! [`keepalive`]. The policy is stated once here either way, so a silent host is
//! noticed on the same schedule whichever protocol is carrying it and the number
//! [`keepalive_budget`] quotes to the user cannot drift from one of them.

use std::future::Future;
use std::time::Duration;

use log::warn;
use tokio::net::TcpStream;

use crate::encode::TileSink;
use crate::protocol::ServerMsg;

/// Idle time before the kernel starts probing a silent peer.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(10);
/// Gap between probes once they have started.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Unanswered probes before the socket fails.
const KEEPALIVE_RETRIES: u32 = 3;

/// Linux only in effect, and the half that is easy to leave out.
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
/// keeps the retransmission budget for a busy socket, which runs to about fifteen
/// minutes.
///
/// No longer behind a `cfg`: the RDP engine hands it to FreeRDP on every platform
/// and FreeRDP applies it only where the option exists, so a `cfg` here would
/// move the same decision into [`keepalive`] and duplicate it.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the TCP connect itself may take.
///
/// A host that is switched off swallows SYNs, and the kernel's own retry budget
/// runs to about two minutes with the client showing "Connecting…" for all of
/// it — no client has a timeout of its own. Generous enough to cross a slow VPN.
///
/// Public because the RDP engine does not make this connection itself: it passes
/// the number to FreeRDP as `TcpConnectTimeout`, so a switched-off host is still
/// reported as a connect failure there rather than as a stalled handshake.
pub const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a protocol handshake may take once the TCP connect has succeeded.
///
/// A host that accepts the connection and then says nothing is a hang no socket
/// timeout catches. Long enough for CredSSP or a DES challenge on a loaded server.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a silent host takes to be noticed: idle, then every probe.
///
/// Exposed so an error message quoting the number cannot drift from the
/// constants behind it.
pub fn keepalive_budget() -> Duration {
    KEEPALIVE_IDLE + KEEPALIVE_INTERVAL * KEEPALIVE_RETRIES
}

/// The same policy, restated for an engine that does not own its socket.
///
/// FreeRDP applies `TCP_KEEPIDLE`, `TCP_KEEPINTVL`, `TCP_KEEPCNT` and — on Linux
/// — `TCP_USER_TIMEOUT` in `libfreerdp/core/tcp.c`, from settings rather than
/// from a `TcpStream` this process configured. So the RDP path cannot call
/// [`tcp_connect`]; it asks for the same thing in the other vocabulary, and
/// building that here is what keeps the two from drifting apart.
///
/// `WRITE_TIMEOUT` is passed on every platform rather than behind a `cfg`,
/// because FreeRDP ignores it where the option does not exist — the same place
/// `arm_liveness_probes` would have had to `cfg` it out.
pub fn keepalive() -> freerdp::KeepAlive {
    freerdp::KeepAlive {
        idle: KEEPALIVE_IDLE,
        interval: KEEPALIVE_INTERVAL,
        retries: KEEPALIVE_RETRIES,
        ack_timeout: WRITE_TIMEOUT,
    }
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
/// wedges behind a kernel which still answers reads as an idle desktop, and RFB
/// offers no probe to close that gap.
pub async fn tcp_connect(dest: &str) -> anyhow::Result<TcpStream> {
    let stream = tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(dest))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "TCP connect to {dest}: no answer after {}s",
                TCP_CONNECT_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| anyhow::anyhow!("TCP connect to {dest}: {e}{}", local_network_hint(&e)))?;
    // Input events are tiny and latency-critical; never coalesce them.
    stream.set_nodelay(true).ok();
    if let Err(e) = arm_liveness_probes(&stream) {
        // Not a reason to refuse a session that otherwise works, but said out
        // loud rather than swallowed: this is a guarantee we no longer have.
        warn!("engine: could not arm TCP keepalive for {dest}: {e}");
    }
    Ok(stream)
}

/// Connect to a remote and run its handshake, reporting any failure to the client.
///
/// The two engines that need this had the same fifteen lines each: bound the
/// handshake, warn, send the `ServerMsg::Error` the picker will show, and give up.
/// `None` means the caller has nothing left to do — it has already been reported.
///
/// The budgets are sequential on purpose, and this is the reason the helper takes
/// a closure rather than a future: the TCP connect has its own deadline inside
/// [`tcp_connect`], and wrapping both in one timeout meant a slow connect ate the
/// handshake's time and was then reported as a handshake that took the full
/// budget. Now a host that is slow to answer is a connect failure, and only what
/// happens after the socket is up is measured against `budget`.
///
/// `protocol` is the log-line prefix (`"rdp"`); the client-facing message
/// uppercases it, which is the form both engines already used.
pub async fn connect_and_handshake<T, F, Fut>(
    protocol: &str,
    dest: &str,
    budget: Duration,
    sink: &TileSink,
    handshake: F,
) -> Option<T>
where
    F: FnOnce(TcpStream) -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let report = async |message: String| {
        let _ = sink.msg(ServerMsg::Error { message }).await;
    };
    let stream = match tcp_connect(dest).await {
        Ok(stream) => stream,
        Err(e) => {
            warn!("{protocol}: connect failed: {e:#}");
            report(format!("{} connect failed: {e}", protocol.to_uppercase())).await;
            return None;
        }
    };
    match tokio::time::timeout(budget, handshake(stream)).await {
        Ok(Ok(value)) => Some(value),
        Ok(Err(e)) => {
            warn!("{protocol}: connect failed: {e:#}");
            report(format!("{} connect failed: {e}", protocol.to_uppercase())).await;
            None
        }
        Err(_) => {
            warn!("{protocol}: handshake with {dest} timed out");
            report(format!(
                "{} connect failed: {dest} did not finish the handshake within {}s",
                protocol.to_uppercase(),
                budget.as_secs()
            ))
            .await;
            None
        }
    }
}

/// What to add to a connect error that a permission could be behind.
///
/// macOS 15 and later refuse an app's connections to anything off this machine
/// until local network access is allowed, and the refusal is `EHOSTUNREACH` —
/// exactly what an address with no route gives. Nothing on this side can tell the
/// two apart, and there is no API that would: TN3179 says so, and it is still
/// saying so.
///
/// So this does not decide; it *mentions*. The error keeps naming what happened
/// and gains one clause naming the cause a user can act on, leaving the address as
/// the other. That is worth the sentence because the permission is invisible from
/// here: a fresh install refuses every target on this Mac, identically, with a
/// message that would otherwise send the reader to check a network that is fine.
///
/// Empty everywhere else, where an unreachable address is simply unreachable.
/// The sentence itself, empty off macOS — where an unreachable address is simply
/// unreachable and no permission stands between the two.
///
/// A constant rather than a second function because there are two callers now
/// that decide differently: [`tcp_connect`] has an `io::Error` to read a kind
/// off, and the RDP engine has FreeRDP's own error, which knows the same thing
/// through [`freerdp::Error::is_unreachable`]. What must not be duplicated is the
/// sentence.
#[cfg(target_os = "macos")]
pub const LOCAL_NETWORK_HINT: &str = ". If this is the app's own gateway, check that remotex is \
     allowed under System Settings > Privacy & Security > Local Network — until it is, every \
     connection off this Mac fails exactly like this";

/// See the macOS half. No other platform gates a connection on a user decision.
#[cfg(not(target_os = "macos"))]
pub const LOCAL_NETWORK_HINT: &str = "";

fn local_network_hint(e: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    if matches!(
        e.kind(),
        ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable | ErrorKind::NetworkDown
    ) {
        LOCAL_NETWORK_HINT
    } else {
        ""
    }
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
    use tokio::sync::mpsc;

    use super::*;

    /// A sink and the channel behind it. `TileSink` forwards through a task of its
    /// own, so a test reads the channel only after [`TileSink::flush`].
    fn sink() -> (TileSink, mpsc::Receiver<ServerMsg>) {
        let (frame_tx, frame_rx) = mpsc::channel(4);
        let plan = crate::config::RenderPlan::Tiles {
            base: crate::config::TileCodec::Png,
            motion: None,
            debug: false,
            adaptive: None,
        };
        let feedback = std::sync::Arc::new(crate::feedback::LinkFeedback::new());
        (TileSink::new("test", frame_tx, plan, feedback), frame_rx)
    }

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
        // socket2 sets these through `SIO_KEEPALIVE_VALS` on Windows, which has no getter;
        // the retries one reads back everywhere.
        #[cfg(not(windows))]
        assert_eq!(socket.tcp_keepalive_time().unwrap(), KEEPALIVE_IDLE);
        #[cfg(not(windows))]
        assert_eq!(socket.tcp_keepalive_interval().unwrap(), KEEPALIVE_INTERVAL);
        assert_eq!(socket.tcp_keepalive_retries().unwrap(), KEEPALIVE_RETRIES);
        #[cfg(target_os = "linux")]
        assert_eq!(socket.tcp_user_timeout().unwrap(), Some(WRITE_TIMEOUT));
        assert!(stream.nodelay().unwrap(), "input events must not coalesce");

        let _ = accept.await.unwrap();
    }

    /// A listener that accepts one connection and holds it, so a handshake can be
    /// exercised without a real server behind it.
    ///
    /// The returned handle owns the accepted socket and parks: dropping the socket
    /// would race the handshake with an EOF. Abort the handle to release both.
    async fn accepting_listener() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dest = listener.local_addr().unwrap().to_string();
        let accept = tokio::spawn(async move {
            let _stream = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        (dest, accept)
    }

    #[tokio::test]
    async fn a_completed_handshake_passes_its_value_through() {
        let (dest, accept) = accepting_listener().await;
        let (sink, mut frame_rx) = sink();

        let value = connect_and_handshake("test", &dest, HANDSHAKE_TIMEOUT, &sink, |_stream| {
            std::future::ready(Ok(7u8))
        })
        .await;

        assert_eq!(value, Some(7));
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "nothing to report");
        accept.abort();
    }

    // The bug this helper's shape exists for: with one timeout around both
    // phases, a slow connect ate the handshake's budget and was then reported as
    // a handshake that had run for the whole of it.
    #[tokio::test]
    async fn a_stalled_handshake_is_reported_against_its_own_budget() {
        let (dest, accept) = accepting_listener().await;
        let (sink, mut frame_rx) = sink();

        let value: Option<()> = connect_and_handshake(
            "test",
            &dest,
            Duration::from_millis(50),
            &sink,
            |_stream| std::future::pending(),
        )
        .await;

        assert!(value.is_none());
        sink.flush().await;
        let ServerMsg::Error { message } = frame_rx.try_recv().unwrap() else {
            panic!("expected an error for the picker");
        };
        assert!(message.contains("did not finish the handshake"), "{message}");
        // Uppercased for the client, as both engines already spelled it.
        assert!(message.starts_with("TEST connect failed:"), "{message}");
        accept.abort();
    }

    #[tokio::test]
    async fn a_connect_that_never_lands_is_not_reported_as_a_handshake_failure() {
        // A port that was just released, so the connect is refused rather than
        // being left to the connect timeout.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dest = listener.local_addr().unwrap().to_string();
        drop(listener);
        let (sink, mut frame_rx) = sink();

        let value: Option<()> = connect_and_handshake(
            "test",
            &dest,
            HANDSHAKE_TIMEOUT,
            &sink,
            |_stream| std::future::ready(Ok(())),
        )
        .await;

        assert!(value.is_none());
        sink.flush().await;
        let ServerMsg::Error { message } = frame_rx.try_recv().unwrap() else {
            panic!("expected an error for the picker");
        };
        assert!(message.contains("TCP connect to"), "{message}");
        assert!(
            !message.contains("handshake"),
            "a connect failure must not read as a handshake one: {message}"
        );
    }

    /// The hint is mentioned, never concluded — so it must appear for the refusal
    /// the permission produces and for nothing else, or it becomes noise on every
    /// unrelated failure.
    #[test]
    fn only_an_unreachable_network_earns_the_permission_hint() {
        use std::io::ErrorKind;
        for quiet in [ErrorKind::ConnectionRefused, ErrorKind::TimedOut, ErrorKind::ConnectionReset]
        {
            assert!(
                local_network_hint(&std::io::Error::from(quiet)).is_empty(),
                "{quiet:?} is a decided answer and needs no advice"
            );
        }
        // The three the gate can produce — and only where the gate exists.
        for kind in
            [ErrorKind::HostUnreachable, ErrorKind::NetworkUnreachable, ErrorKind::NetworkDown]
        {
            let hint = local_network_hint(&std::io::Error::from(kind));
            assert_eq!(
                !hint.is_empty(),
                cfg!(target_os = "macos"),
                "{kind:?} gave {hint:?}"
            );
            if !hint.is_empty() {
                assert!(hint.contains("Local Network"), "{hint}");
            }
        }
    }

    /// A refused port is the common failure, and the one an unasked-for sentence
    /// about permissions would be wrong about.
    #[tokio::test]
    async fn a_refused_connection_is_reported_without_the_hint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dest = listener.local_addr().unwrap().to_string();
        drop(listener);

        let Err(failed) = tcp_connect(&dest).await else {
            panic!("a port with nothing behind it connected");
        };

        let message = format!("{failed:#}");
        assert!(message.contains("TCP connect to"), "{message}");
        assert!(!message.contains("Local Network"), "{message}");
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
