# Phase 2 — Consolidating VNC (remotex) and RDP

> **Status: proposal.** This captures the architecture decision for bringing the
> `../remotex` VNC client and this RDP client under one roof. Nothing here is
> implemented yet.

## Question

Should we fold `../remotex` (a VNC/RFB client in TypeScript/Bun, with the
protocol engine running **in the browser** behind a dumb-pipe server) into this
project so a single Rust backend and a single frontend serve **both** RDP and
VNC? Or keep them as separate projects?

The specific worry: does moving VNC onto a Rust backend degrade the VNC
experience?

## Decision

**Consolidate the shell; do not move the VNC protocol engine server-side.**

- One Rust backend and one frontend app, shared.
- RDP keeps its **server-side** engine (IronRDP), as today — it has to: RDP is
  stateful, credential-bearing, and complex.
- VNC is ported into this backend as a **dumb-pipe relay** (raw RFB bytes
  relayed between the VNC TCP socket and the browser WebSocket); the **browser**
  keeps decoding RFB, exactly as remotex does today.

This gets the consolidation benefits without risking VNC quality.

## The two independent axes

"Same project" and "same architecture" are separable decisions:

1. **Where the shell lives** — axum server, `rust-embed`, the WebSocket bridge,
   auth, config, session lifecycle, and the frontend chrome (connection UI,
   input overlay, status bar, canvas). **High value to unify, low risk.**
2. **Where the protocol engine decodes** — browser-side (VNC today) vs.
   server-side (RDP today). **This is the axis that carries the risk.**

We unify axis 1 fully and leave axis 2 per-protocol.

## Why not server-side VNC decode

There are two ways to "move VNC onto the Rust backend":

| Approach | What Rust does | Client link | Verdict |
| --- | --- | --- | --- |
| **Dumb-pipe relay** | Relays raw RFB bytes TCP↔WS; browser decodes | Keeps VNC's compact Tight/ZRLE encodings, decoded natively in the browser | **No degradation.** A Rust relay is leaner than the Bun one. |
| **Server-side decode** | Decodes RFB → re-encodes → sends to browser | Throws away VNC's wire encodings; must send raw RGBA (bandwidth blowup) or re-compress to PNG/H.264 (CPU + latency) | **Degrades VNC** unless paired with a serious re-encoding investment. Not worth it for consolidation alone. |

So VNC becomes a dumb-pipe relay in this backend and keeps its browser-side
engine. The RDP/VNC asymmetry (RDP server-side, VNC browser-side) is normal and
correct — it reflects the protocols, not an inconsistency to fix.

## What "one frontend" actually buys

Honest scope: the frontend only *partly* unifies.

- **Shared:** the app shell, connection flow, input capture, status/layout,
  keyboard handling, the WebSocket plumbing.
- **Not shared:** the render path. RDP uses the tile-blitter (`putImageData` of
  server-decoded RGBA tiles). VNC dumb-pipe needs the RFB decoder (ported from
  remotex / noVNC).

So it is "one app, two pluggable renderers selected at runtime," not one
renderer. Still a substantial win — just not total.

## Proposed seam

### Backend

A `Session` trait abstracts "connect, produce frames, accept input", with the
transport model differing per implementation:

```
trait Session {
    async fn connect(config) -> Result<Self>;
    // RDP: emits ServerMsg::Tile (server-decoded).
    // VNC: relays raw RFB frames as binary WS messages (browser decodes).
    // input flows the other way as ClientMsg / raw RFB.
}

rdp::Session   // IronRDP, server-side decode → tiles           [feature = "rdp"]
vnc::Session   // TCP↔WS relay, raw RFB passthrough             [feature = "vnc"]
```

Gate the engines behind cargo features so a VNC-only deploy does not pull the
(heavy) IronRDP dependency tree, and an RDP-only deploy does not pull the VNC
bits.

The WebSocket endpoint selects the session type from the connection request
(e.g. `/ws?kind=rdp|vnc` or a `ClientMsg::Connect { kind, target }` handshake).
Credentials stay server-side for both.

### Frontend

```
<RemoteSession kind="rdp" | "vnc">
    ├─ TileRenderer   // current canvas putImageData path (RDP)
    └─ RfbRenderer    // ported from remotex (VNC, browser-side decode)
```

Shared shell components (status bar, input overlay, connection form) wrap
whichever renderer the session kind selects. The `useRemoteDesktop` hook splits
into a shared transport/input layer plus a per-kind frame handler.

## Repo layout

Lean toward a **monorepo** with a shared backend library crate and shared
frontend components, feature-gated:

- One deployable; shared auth/config/transport; the `Session` trait keeps the
  two engines honest.
- Cost: build weight and coupling — mitigated by cargo features.
- Go **separate** only if the two will have genuinely divergent deploy targets
  or ownership.

## When server-side VNC decode *would* earn its place

Reserve full server-side VNC decode for a later, deliberate project — not this
consolidation. It pays off only with a concrete motivation, e.g.:

- A truly thin client (no protocol engine in the browser at all).
- A single uniform codec (H.264 / WebCodecs) for **both** protocols.
- Server-side session recording.
- Clientless, server-enforced bandwidth control.

If pursued, do it for RDP and VNC together, and measure the latency/bandwidth
trade against the dumb-pipe baseline before committing.

## Migration sketch (rough order)

1. Extract this project's shell into a backend library crate (router, embed, ws
   bridge, config, session lifecycle) with an `rdp` feature.
2. Introduce the `Session` trait; make the current RDP path implement it.
3. Add a `vnc` feature with a dumb-pipe `Session` (port remotex's server relay).
4. Split the frontend into shell + `TileRenderer`; port remotex's RFB engine as
   `RfbRenderer`; select by session kind.
5. Add the connection handshake (kind + target) and per-kind credential config.
6. Measure both paths; only then evaluate whether any server-side-decode work is
   worthwhile.
