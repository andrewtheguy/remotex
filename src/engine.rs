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
/// keeps the retransmission budget for a busy socket, which runs to about fifteen
/// minutes.
#[cfg(target_os = "linux")]
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the TCP connect itself may take.
///
/// A host that is switched off swallows SYNs, and the kernel's own retry budget
/// runs to about two minutes with the client showing "Connecting…" for all of
/// it — no client has a timeout of its own. Generous enough to cross a slow VPN.
///
/// It is also how long there is to answer the macOS sheet described at
/// [`awaiting_local_network_permission`], which is the other thing a connect can
/// now be waiting for.
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait before trying again while the local network sheet is open.
///
/// Only ever reached where [`awaiting_local_network_permission`] can be true.
/// Short enough that the desktop appears as soon as the sheet is answered rather
/// than up to a second later, long enough that waiting is not a spin.
const NO_ROUTE_RETRY: Duration = Duration::from_millis(250);

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
/// neither RFB nor IronRDP offers a probe to close that gap.
///
/// The retry loop is for one specific refusal and no other — see
/// [`awaiting_local_network_permission`]. Every other error is still reported the
/// moment it happens, so a mistyped address does not become a twenty-second wait.
pub async fn tcp_connect(dest: &str) -> anyhow::Result<TcpStream> {
    let deadline = tokio::time::Instant::now() + TCP_CONNECT_TIMEOUT;
    // Whether anything was ever refused for want of a route, which is what decides
    // *which* explanation the budget running out deserves.
    let mut no_route = false;
    let stream = loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            anyhow::bail!("{}", out_of_time(dest, no_route));
        }
        match tokio::time::timeout(left, TcpStream::connect(dest)).await {
            Ok(Ok(stream)) => break stream,
            Ok(Err(e)) if awaiting_local_network_permission(&e) => {
                if !no_route {
                    // Once, not per attempt: a log line every 250 ms would bury the
                    // one thing worth reading here.
                    warn!(
                        "engine: no route to {dest} yet — if this is a first run, macOS \
                         is asking whether to allow local network access. Waiting up to \
                         {}s for an answer.",
                        TCP_CONNECT_TIMEOUT.as_secs()
                    );
                }
                no_route = true;
                tokio::time::sleep(NO_ROUTE_RETRY).await;
            }
            Ok(Err(e)) => anyhow::bail!("TCP connect to {dest}: {e}"),
            Err(_) => anyhow::bail!("{}", out_of_time(dest, no_route)),
        }
    };
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

/// Whether a refused connect is one the user is about to be asked to allow.
///
/// macOS 15 and later gate an app's access to anything off this machine behind a
/// Local Network sheet, and until that sheet is answered the kernel refuses every
/// connection with `EHOSTUNREACH` — the *same* error a genuinely unrouteable
/// address gives, with no way to tell them apart. So the first target picked after
/// a fresh install failed instantly with "No route to host" and picking it again
/// worked, which also accounts for one address family appearing to work where
/// another did not: the variable was the attempt, not the address.
///
/// Waiting is enough, and that is the measured part: on macOS 26 a process that
/// was *already running* when the sheet was answered connects on its very next
/// attempt, so nothing has to be restarted or re-execed. The sheet covers the
/// embedded gateway too — the permission belongs to the responsible app bundle,
/// and `remotex-gateway` is a child of `remotex.app`.
///
/// Everywhere else there is no such gate and an unreachable address is simply
/// unreachable, so it is reported at once rather than waited on.
#[cfg(target_os = "macos")]
fn awaiting_local_network_permission(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(e.kind(), ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable)
}

/// See the macOS half. No other platform gates a local connection on a user
/// decision, so there is nothing here that waiting would fix.
#[cfg(not(target_os = "macos"))]
fn awaiting_local_network_permission(_: &std::io::Error) -> bool {
    false
}

/// What to say when the connect budget ran out, which depends on how it ran out.
///
/// A host that swallowed every SYN and a host the kernel would not route to are
/// different problems with different answers, and the old message only knew about
/// the first.
fn out_of_time(dest: &str, no_route: bool) -> String {
    let secs = TCP_CONNECT_TIMEOUT.as_secs();
    if no_route {
        format!(
            "TCP connect to {dest}: no route for {secs}s. On macOS an app cannot reach \
             anything off this machine until local network access is allowed — answer \
             the sheet, or turn remotex on under System Settings > Privacy & Security > \
             Local Network. If it is already on, check the address"
        )
    } else {
        format!("TCP connect to {dest}: no answer after {secs}s")
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
        };
        (TileSink::new("test", frame_tx, plan), frame_rx)
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
        assert_eq!(socket.tcp_keepalive_time().unwrap(), KEEPALIVE_IDLE);
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

    /// The guard on the retry loop's blast radius: only the one refusal macOS can
    /// change its mind about is waited on.
    #[test]
    fn only_the_refusal_a_user_can_answer_is_waited_on() {
        use std::io::ErrorKind;
        let refused = std::io::Error::from(ErrorKind::ConnectionRefused);
        assert!(
            !awaiting_local_network_permission(&refused),
            "a closed port is a decided answer, not a pending one"
        );
        assert!(!awaiting_local_network_permission(&std::io::Error::from(ErrorKind::TimedOut)));
        // The two the sheet produces — and only where the sheet exists.
        let macos = cfg!(target_os = "macos");
        for kind in [ErrorKind::HostUnreachable, ErrorKind::NetworkUnreachable] {
            assert_eq!(
                awaiting_local_network_permission(&std::io::Error::from(kind)),
                macos,
                "{kind:?}"
            );
        }
    }

    /// The regression this fix could have introduced: turning *every* failure into
    /// a twenty-second wait. A refused port must still be answered at once.
    #[tokio::test]
    async fn a_refused_connection_is_reported_without_spending_the_budget() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dest = listener.local_addr().unwrap().to_string();
        drop(listener);

        let started = std::time::Instant::now();
        let failed = tcp_connect(&dest).await;

        assert!(failed.is_err(), "a port with nothing behind it connected");
        // Not a timing measurement: a refused connect is one round trip to the
        // loopback, so any machine that finishes this at all finishes it in a
        // small fraction of the budget. What fails here is a *retry*, which
        // cannot come in under the budget at all.
        assert!(
            started.elapsed() < TCP_CONNECT_TIMEOUT / 2,
            "a refused connect was retried rather than reported"
        );
    }

    #[test]
    fn the_reason_the_budget_ran_out_reaches_the_message() {
        let silent = out_of_time("10.0.0.2:3389", false);
        assert!(silent.contains("no answer after 20s"), "{silent}");
        assert!(!silent.contains("Local Network"), "a silent host is not a permission: {silent}");

        let unrouted = out_of_time("10.0.0.2:3389", true);
        // Both ways out, because either can be the true one and the message is all
        // the user gets: the permission, and an address that is simply wrong.
        assert!(unrouted.contains("Local Network"), "{unrouted}");
        assert!(unrouted.contains("check the address"), "{unrouted}");
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
