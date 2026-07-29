//! What a full-desktop repaint costs on a real macOS agent, and how much of it the
//! encoder can be made to overlap.
//!
//! An instrument, not a test: it asserts nothing. It is `rdp_repaint_probe`'s opposite
//! number, and exists for the same reason — to pick and defend a dial that cannot be
//! reasoned out, here `session::ENCODE_WIDTH`. A `Refresh` forgets the cell memo and
//! repaints the whole surface, so it is one frame's worth of cells pushed in a tight
//! loop: 320 of them on a 2x 1600x1000 display, which is the workload the width is for.
//!
//! It dials the **agent** directly rather than going through a gateway, the way
//! `rxa_resize_e2e` and `rxa_host_scale_e2e` do and with the same environment, because
//! everything a gateway would add here is between the measurement and the thing
//! measured:
//!
//!     REMOTEX_RXA_HOST='[fdb8:…:a:20]:52381' \
//!     REMOTEX_RXA_PRIVATE_KEY=rxgs… \
//!     REMOTEX_RXA_AGENT_PUBLIC_KEY=rxap… \
//!       cargo test --release --test rxa_repaint_probe -- --ignored --nocapture
//!
//! `--release` is for the probe's own decode-free path rather than for the agent, which
//! is whatever was installed on the Mac — so **the agent must be a release build too**,
//! and it is the one number that does not come out of this repo's build.
//!
//! Read two things out of a run: this probe's `PROBE:` line for wall clock per repaint,
//! and `encoder: encode totals:` in the agent's own log
//! (`~/Library/Logs/remotex-agent.log`), which it prints when this connection drops.
//! In those totals `encoding across workers` against `of waiting` is the parallelism
//! actually achieved, and **`stalled`** is time the encoder spent handing tiles to the
//! socket — if that dominates, no width can make a repaint faster and the link is the
//! constraint.
//!
//! Two runs only compare at the same surface size, so the line prints what it measured.
//! Byte counts *are* comparable here in a way `rdp_repaint_probe`'s are not — the agent
//! is idle apart from its own menu bar clock — but the tile count is the surer check
//! that two runs did the same work.

use std::time::Duration;

use rxa_proto::msg::{AgentMsg, GatewayMsg};
use tokio::net::TcpStream;

/// How long to hold the agent under repaint pressure and count what comes back.
///
/// A window rather than "time each repaint until the stream goes quiet", which is what
/// this probe first tried and what `rdp_repaint_probe` can do. It does not work here,
/// and the reason is the finding rather than an inconvenience: at a slow width a
/// 320-cell repaint outlives the two-frame raw channel, so capture drops a frame and
/// sets `full_repaint`, which asks for another 320-cell repaint. The stream never goes
/// quiet at all. Sustained throughput is therefore the honest measure, and the ratio
/// the agent's own totals print — tiles over frames — says whether that loop is still
/// running: near 320 means every frame is a full repaint, and a small number means
/// capture is keeping up and reporting real damage again.
const WINDOW: Duration = Duration::from_secs(20);

/// How often to ask for a full repaint inside the window.
///
/// Also what keeps the session alive: the agent drops a gateway that says nothing for
/// 45s, and this probe's only traffic is these.
const REFRESH_EVERY: Duration = Duration::from_millis(250);

fn key<T>(name: &str, parse: impl FnOnce(&str) -> Option<T>) -> T {
    let text = std::env::var(name).unwrap_or_else(|_| panic!("{name} is unset"));
    parse(&text).unwrap_or_else(|| panic!("{name} is not a key of the right kind"))
}

#[tokio::test]
#[ignore = "instrument, not a test; needs a real macOS agent"]
async fn full_desktop_repaints_against_a_real_agent() {
    use rxa_proto::key::{Role, parse_private, parse_public};

    let host = std::env::var("REMOTEX_RXA_HOST").expect("REMOTEX_RXA_HOST is unset");
    let private = key("REMOTEX_RXA_PRIVATE_KEY", |t| parse_private(Role::Gateway, t).ok());
    let agent_public = key("REMOTEX_RXA_AGENT_PUBLIC_KEY", |t| parse_public(Role::Agent, t).ok());

    let mut stream = TcpStream::connect(&host).await.expect("connect to the agent");
    stream.set_nodelay(true).ok();
    let transport = rxa_proto::noise::initiate(&mut stream, &private, &agent_public)
        .await
        .expect("handshake (are the two keys a pair?)");
    let (read_half, write_half) = stream.into_split();
    let (mut reader, mut writer) = rxa_proto::frame::split(read_half, write_half, transport);

    writer.send(&GatewayMsg::Attach.encode()).await.expect("attach");

    // Let the first repaint — the one that comes with attaching — finish and the
    // surface settle, so what is measured is a repaint of a finished screen. The size
    // comes out of this window too, from whichever of `Hello`/`DisplaySize` arrives.
    let mut size = (0u16, 0u16);
    let settle = tokio::time::Instant::now() + Duration::from_secs(6);
    while tokio::time::Instant::now() < settle {
        if let Ok(Ok(bytes)) = tokio::time::timeout(Duration::from_millis(250), reader.recv()).await
        {
            match AgentMsg::decode(&bytes) {
                Ok(AgentMsg::Hello { w, h, .. } | AgentMsg::DisplaySize { w, h, .. }) => {
                    size = (w, h);
                }
                // The likeliest place for one, since this is where capture starts: a
                // missing Screen Recording grant arrives here. Silently ignored it
                // would leave a reading of nearly no tiles and no reason for it.
                Ok(AgentMsg::Error { message }) => panic!("the agent reported: {message}"),
                _ => {}
            }
        }
    }

    let mut tiles = 0u64;
    let mut bytes_total = 0u64;
    let mut refreshes = 0u64;
    let started = tokio::time::Instant::now();
    let deadline = started + WINDOW;
    let mut next_refresh = started;
    while tokio::time::Instant::now() < deadline {
        if tokio::time::Instant::now() >= next_refresh {
            writer.send(&GatewayMsg::Refresh.encode()).await.expect("refresh");
            refreshes += 1;
            next_refresh += REFRESH_EVERY;
        }
        match tokio::time::timeout(REFRESH_EVERY, reader.recv()).await {
            Ok(Ok(frame)) => match AgentMsg::decode(&frame) {
                Ok(AgentMsg::Tile { data, .. }) => {
                    tiles += 1;
                    bytes_total += data.len() as u64;
                }
                // A capture that died and did not come back, most often. Fatal here
                // rather than counted: everything after it is measuring nothing.
                Ok(AgentMsg::Error { message }) => panic!("the agent reported: {message}"),
                _ => {}
            },
            Ok(Err(e)) => panic!("the agent closed mid-probe: {e}"),
            // Nothing for a refresh interval, which at a fast width means the agent
            // has run out of damage to report. Not an error, just a quiet moment.
            Err(_) => {}
        }
    }
    let elapsed = started.elapsed().as_secs_f64();

    // Dropping the connection is what ends the agent's session, and a session that
    // ends is what logs `encode totals` — the other half of the reading.
    drop(reader);
    drop(writer);

    println!(
        "PROBE: {}x{} under {refreshes} refreshes over {elapsed:.1}s: \
         {tiles} tiles ({:.0}/s) / {bytes_total} bytes ({:.1} MB/s)",
        size.0,
        size.1,
        tiles as f64 / elapsed,
        bytes_total as f64 / elapsed / 1_048_576.0,
    );
}
