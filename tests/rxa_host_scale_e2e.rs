//! `HostScale` against a **real** macOS agent with a display of its own.
//!
//! Ignored by default and driven by environment variables, because the thing
//! under test is the one part of the density path that no fake can stand in for:
//! whether `applySettings:` on a live `CGVirtualDisplay` actually changes its
//! backing scale, and whether the agent then measures and reports the new one.
//! Everything either side of that is covered without a Mac in `rxa_e2e.rs`.
//!
//!     REMOTEX_RXA_HOST='...:52381' \
//!     REMOTEX_RXA_PRIVATE_KEY=rxgs… \
//!     REMOTEX_RXA_AGENT_PUBLIC_KEY=rxap… \
//!     cargo test --test rxa_host_scale_e2e -- --ignored --nocapture
//!
//! The agent must have `virtual_display = true` and be otherwise idle: it serves
//! one gateway at a time, so a session already attached is evicted by this one.

use std::time::Duration;

use rxa_proto::msg::{AgentMsg, DisplayEntry, GatewayMsg, SCALE_ONE};
use tokio::net::TcpStream;

/// Long enough for `applySettings:` plus the WindowServer settling behind it,
/// measured at 66–397 ms to apply and 134–580 ms to settle.
const SETTLE: Duration = Duration::from_secs(5);

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
}

/// The agent's own display, and the scale it is currently reporting for it.
async fn owned_display(agent: &mut Agent) -> Option<DisplayEntry> {
    agent
        .wait_for(|msg| match msg {
            AgentMsg::Displays { displays, .. } => {
                displays.iter().find(|d| d.is_owned()).cloned()
            }
            _ => None,
        })
        .await
}

#[tokio::test]
#[ignore = "needs a real macOS agent with a virtual display; see the module docs"]
async fn the_agents_display_follows_the_clients_density() {
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

    let owned = owned_display(&mut agent).await.expect("a display of the agent's own");
    println!("owned display {} — {} ({})", owned.id, owned.detail, owned.scale);

    // Share it: the density only follows a client while the display it would
    // change is the one being looked at. No `DisplaySize` is expected in reply —
    // the agent announces one only when the target actually changes, and on a Mac
    // where the agent's display is the main one the session already started here.
    agent.send(GatewayMsg::SelectDisplay { id: owned.id }).await;
    agent.send(GatewayMsg::Attach).await;
    agent
        .wait_for(|msg| matches!(msg, AgentMsg::Tile { .. }).then_some(()))
        .await
        .expect("a keyframe tile, so the capture is live");

    // A client on a 1x screen: the display should drop to 1x, halving each axis
    // of the framebuffer, and say so.
    agent.send(GatewayMsg::HostScale { scale: SCALE_ONE }).await;
    let at_1x = agent
        .wait_for(|msg| match msg {
            AgentMsg::DisplaySize { w, h, scale } => Some((*w, *h, *scale)),
            _ => None,
        })
        .await
        .expect("a size after asking for 1x");
    println!("after HostScale(100): {}x{} at {}", at_1x.0, at_1x.1, at_1x.2);
    assert_eq!(at_1x.2, SCALE_ONE, "the display should report 1x");

    // And back: a client on a Retina screen gets the 2x desktop again, at the
    // same point size it had all along.
    agent
        .send(GatewayMsg::HostScale {
            scale: 2 * SCALE_ONE,
        })
        .await;
    let at_2x = agent
        .wait_for(|msg| match msg {
            AgentMsg::DisplaySize { w, h, scale } if *scale == 2 * SCALE_ONE => {
                Some((*w, *h, *scale))
            }
            _ => None,
        })
        .await
        .expect("a size after asking for 2x");
    println!("after HostScale(200): {}x{} at {}", at_2x.0, at_2x.1, at_2x.2);
    assert_eq!(
        (at_2x.0 / 2, at_2x.1 / 2),
        (at_1x.0, at_1x.1),
        "the same desktop in points, with twice the pixels on each axis"
    );
}
