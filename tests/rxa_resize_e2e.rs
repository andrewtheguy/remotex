//! `ResizeDisplay` against a **real** macOS agent with a display of its own.
//!
//! Ignored by default and driven by environment variables, for the same
//! reason as `rxa_host_scale_e2e.rs`: the thing under test is the part no fake
//! can stand in for — whether `applySettings:` at a new point size actually
//! moves a live `CGVirtualDisplay`, whether the density it was in survives, and
//! whether the agent then measures and reports what it landed on. The routing
//! either side of that is covered without a Mac in `rxa_e2e.rs`.
//!
//!     REMOTEX_RXA_HOST='[fdb8:…:a:20]:52381' \
//!     REMOTEX_RXA_PRIVATE_KEY=rxgs… \
//!     REMOTEX_RXA_AGENT_PUBLIC_KEY=rxap… \
//!     cargo test --test rxa_resize_e2e -- --ignored --nocapture
//!
//! The agent must have `virtual_display = true` and be otherwise idle: it serves
//! one gateway at a time, so a session already attached is evicted by this one.
//!
//! Nothing reverts a resize — macOS files it against the display's identity, and
//! that is the intended behaviour — so this test puts the display back where it
//! found it before returning.

use std::time::Duration;

use rxa_proto::msg::{AgentMsg, DisplayEntry, GatewayMsg, SCALE_ONE};
use tokio::net::TcpStream;

/// Long enough for `applySettings:` plus the WindowServer settling behind it,
/// measured at 66–397 ms to apply and 134–580 ms to settle.
const SETTLE: Duration = Duration::from_secs(5);

/// How far to shrink, in points per axis.
///
/// Small on purpose. It has to be inside the envelope, which this wire never
/// states, and comfortably above the roughly-57% floor where the display leaves
/// the HiDPI window — because the assertion that matters here is that the
/// *density did not change*, and a step large enough to cost 2x would make that
/// assertion fail for a reason that is correct behaviour.
const STEP_POINTS: u16 = 64;

/// The two halves of the pairing this test dials with: a gateway private key
/// standing in for `[rxa].private_key`, and the Mac's public key from its own
/// Settings or `remotex-agent --public-key`.
struct Keys {
    private: [u8; 32],
    agent_public: [u8; 32],
}

impl Keys {
    fn from_env() -> Self {
        use rxa_proto::key::{Role, parse_private, parse_public};

        let read = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is unset"));
        Self {
            private: parse_private(Role::Gateway, &read("REMOTEX_RXA_PRIVATE_KEY"))
                .expect("REMOTEX_RXA_PRIVATE_KEY"),
            agent_public: parse_public(Role::Agent, &read("REMOTEX_RXA_AGENT_PUBLIC_KEY"))
                .expect("REMOTEX_RXA_AGENT_PUBLIC_KEY"),
        }
    }
}

struct Agent {
    reader: rxa_proto::frame::FrameReader<tokio::net::tcp::OwnedReadHalf>,
    writer: rxa_proto::frame::FrameWriter<tokio::net::tcp::OwnedWriteHalf>,
}

impl Agent {
    async fn connect(host: &str, keys: &Keys) -> Self {
        let mut stream = TcpStream::connect(host).await.expect("connect to the agent");
        stream.set_nodelay(true).ok();
        let transport = rxa_proto::noise::initiate(&mut stream, &keys.private, &keys.agent_public)
            .await
            .expect("handshake (are the two keys a pair?)");
        let (read_half, write_half) = stream.into_split();
        let (reader, writer) = rxa_proto::frame::split(read_half, write_half, transport);
        Self { reader, writer }
    }

    async fn send(&mut self, msg: GatewayMsg) {
        self.writer.send(&msg.encode()).await.expect("send");
    }

    /// The next message matching `f`, or `None` once `SETTLE` passes.
    ///
    /// Filtered rather than "the next message": a live agent is streaming tiles
    /// and cursor updates the whole time, and this has to see past them.
    async fn wait_for<T>(&mut self, mut f: impl FnMut(&AgentMsg) -> Option<T>) -> Option<T> {
        tokio::time::timeout(SETTLE, async {
            loop {
                let bytes = self.reader.recv().await.ok()?;
                if let Some(found) = f(&AgentMsg::decode(&bytes).ok()?) {
                    return Some(found);
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// The next reported geometry, in **points** and the density behind them.
    ///
    /// Points because that is the unit a resize is asked in, and the wire carries
    /// captured pixels — so every comparison in this file would otherwise be a
    /// division written out again.
    async fn next_size(&mut self) -> Option<((u16, u16), u16)> {
        self.wait_for(|msg| match msg {
            AgentMsg::DisplaySize { w, h, scale } => Some((points(*w, *h, *scale), *scale)),
            _ => None,
        })
        .await
    }
}

/// Captured pixels to display points. `scale` is in [`SCALE_ONE`] hundredths.
fn points(w: u16, h: u16, scale: u16) -> (u16, u16) {
    let ratio = u32::from(scale.max(SCALE_ONE));
    (
        (u32::from(w) * u32::from(SCALE_ONE) / ratio) as u16,
        (u32::from(h) * u32::from(SCALE_ONE) / ratio) as u16,
    )
}

/// The agent's own display, and the geometry it is currently reporting for it.
async fn owned_display(agent: &mut Agent) -> Option<DisplayEntry> {
    agent
        .wait_for(|msg| match msg {
            AgentMsg::Displays { displays, .. } => displays.iter().find(|d| d.is_owned()).cloned(),
            _ => None,
        })
        .await
}

#[tokio::test]
#[ignore = "needs a real macOS agent with a virtual display; see the module docs"]
async fn the_agents_display_follows_the_clients_window() {
    let host = std::env::var("REMOTEX_RXA_HOST").expect("REMOTEX_RXA_HOST");
    let keys = Keys::from_env();

    let mut agent = Agent::connect(&host, &keys).await;
    let hello = agent
        .wait_for(|msg| match msg {
            AgentMsg::Hello { w, h, scale, .. } => Some((*w, *h, *scale)),
            _ => None,
        })
        .await
        .expect("Hello");
    println!("hello: {}x{} at {}", hello.0, hello.1, hello.2);

    let owned = owned_display(&mut agent)
        .await
        .expect("a display of the agent's own");
    println!("owned display {} — {} ({})", owned.id, owned.detail, owned.scale);
    let started_at = points(owned.w, owned.h, owned.scale);
    let started_scale = owned.scale;
    println!("starting at {}x{} points", started_at.0, started_at.1);

    // Share it: the size only follows a client while the display it would change
    // is the one being looked at, exactly as the density does.
    agent.send(GatewayMsg::SelectDisplay { id: owned.id }).await;
    agent.send(GatewayMsg::Attach).await;
    agent
        .wait_for(|msg| matches!(msg, AgentMsg::Tile { .. }).then_some(()))
        .await
        .expect("a keyframe tile, so the capture is live");

    // A window a little smaller than the desktop. Inside the envelope by
    // construction — the display is already at or under it — so this is the case
    // with no clamping in it, and the one that pins "the density is preserved".
    let smaller = (
        started_at.0.saturating_sub(STEP_POINTS).max(800),
        started_at.1.saturating_sub(STEP_POINTS).max(600),
    );
    assert_ne!(
        smaller, started_at,
        "the display is already at the floor; give it a larger initial size to test against"
    );
    agent
        .send(GatewayMsg::ResizeDisplay {
            w: smaller.0,
            h: smaller.1,
        })
        .await;
    let (shrunk, shrunk_scale) = agent.next_size().await.expect("a size after shrinking");
    println!("after shrinking: {}x{} points at {shrunk_scale}", shrunk.0, shrunk.1);
    assert_eq!(shrunk, smaller, "the display should be the size it was asked for");
    assert_eq!(
        shrunk_scale, started_scale,
        "a resize must keep the density the display was in — that is what set_size preserves, \
         and it is the one thing a fake agent cannot check"
    );

    // Past everything. The clamp lands on the size the display was created at,
    // which this wire never states — so the assertion is that it stopped
    // somewhere sane rather than at a number the test knows.
    agent
        .send(GatewayMsg::ResizeDisplay {
            w: u16::MAX,
            h: u16::MAX,
        })
        .await;
    let (envelope, _) = agent.next_size().await.expect("a size after growing");
    println!("the envelope is {}x{} points", envelope.0, envelope.1);
    assert!(
        envelope.0 >= shrunk.0 && envelope.1 >= shrunk.1,
        "growing should not have made the desktop smaller: {envelope:?} vs {shrunk:?}"
    );

    // The same oversized request again, which clamps to the size the display is
    // already in. Nothing should happen at all — and this is the assertion worth
    // having on a real VM, because a display stack that is asked to reconfigure
    // often enough wedges until the guest is rebooted.
    agent
        .send(GatewayMsg::ResizeDisplay {
            w: u16::MAX,
            h: u16::MAX,
        })
        .await;
    assert!(
        agent.next_size().await.is_none(),
        "a request for the size the display is already in must not reconfigure it"
    );

    // Put it back. Nothing reverts a resize — macOS remembers it against the
    // display's identity, which is the intended behaviour — so a test run that
    // stopped here would leave the developer's display somewhere they did not
    // choose.
    //
    // Only if it is not already there. On a display that had been left at its
    // created size, the clamp above restored it as a side effect, and asking for
    // a size the display is already in is exactly the case that reports nothing —
    // as the assertion immediately above just required.
    if envelope != started_at {
        agent
            .send(GatewayMsg::ResizeDisplay {
                w: started_at.0,
                h: started_at.1,
            })
            .await;
        let (restored, _) = agent.next_size().await.expect("a size after restoring");
        assert_eq!(
            restored, started_at,
            "the display should be back where it started"
        );
    }
    println!("left at {}x{} points, where it was found", started_at.0, started_at.1);
}
