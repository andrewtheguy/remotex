# Phase 2 — Consolidating VNC (remotex) and RDP

> **Status: implemented** (all three scope items; see the migration list at
> the bottom for what remains beyond phase 2). This supersedes the earlier
> proposal in this file, which recommended keeping VNC decoding in the browser
> (dumb-pipe relay). That decision is **reversed**: both protocols decode
> **server-side** in Rust, and the browser speaks one uniform protocol.

## Question

Should we fold `../remotex` (a VNC/RFB client in TypeScript/Bun, with the
protocol engine running **in the browser** behind a dumb-pipe server) into this
project so a single Rust backend and a single frontend serve **both** RDP and
VNC — and if so, where does the VNC protocol engine live?

## Decision

**Consolidate fully, with server-side decode for both protocols.**

- One Rust backend, one frontend app.
- RDP keeps its server-side engine (IronRDP), as today.
- VNC gets a **server-side RFB client in Rust**. The browser never sees RFB;
  it receives the same uniform server→browser protocol RDP already uses
  (tiles today, whatever the transport evolves into). remotex's browser-side
  RFB engine (noVNC-derived) is retired, not ported.

## Why server-side decode won

The earlier proposal rejected server-side VNC decode on bandwidth grounds
("throws away VNC's compact wire encodings"). That analysis assumed the
backend↔browser link and the backend↔VNC-server link were the same link. They
are not, and the real constraints point the other way:

1. **The targets are LAN.** This program connects to RDP and VNC destinations
   on the local network. The backend↔VNC-server hop is fast and cheap —
   spending VNC's clever encodings there buys nothing. The actual bottleneck
   is **backend → WAN browser over a weak signal**, and server-side decode
   lets us optimize that one link with a single transport strategy for all
   protocols, independent of what any remote server supports.

2. **Rust decodes cheaply; this is a single-user program.** One session's
   worth of RFB decoding is negligible CPU for the backend. And it lets us
   break away from the legacy noVNC-derived code entirely instead of carrying
   it forward.

3. **It generalizes.** With decode server-side, adding another protocol
   (e.g. RustDesk) later is "write another `Session` impl" — the browser and
   its transport don't change. Browser-side decode would mean a new in-browser
   engine per protocol.

4. **Resume and takeover.** Client-side VNC makes session resume and session
   takeover very flaky: the protocol state lives in the browser tab, so it
   dies with it. With the engine server-side, the backend owns the framebuffer
   and the connection; a browser can detach and reattach (or a second browser
   can take over) without touching the VNC session.

5. **VNC's advanced features are unreliable in practice.** The encoding /
   resize workarounds don't work uniformly across implementations (macOS
   Screen Sharing being the standing example), and papering over that in
   remotex introduced real complexity. Server-side, we do what Guacamole did
   originally: speak the **raw/standard baseline that every VNC server must
   support**, keep the RFB client simple, and put the optimization effort into
   the backend→browser link we control.

The old table, corrected for these constraints:

| Approach | Backend↔VNC link | Backend↔browser link | Verdict |
| --- | --- | --- | --- |
| Dumb-pipe relay (old decision) | Compact wire encodings — wasted on LAN | Whatever the VNC server emits; can't be tuned for weak WAN | Optimizes the wrong link; keeps noVNC legacy; flaky resume |
| **Server-side decode (new decision)** | Raw/standard RFB — trivial cost on LAN | One uniform, tunable protocol shared with RDP | **Chosen.** Simple VNC client, one transport to optimize, clean resume/takeover, extensible |

## Phase 2 scope

Phase 2 prepares this repo to absorb remotex, focused narrowly on the data
path. Explicitly **in**:

1. **Browser↔backend transport efficiency — (done).** This is the bottleneck
   link and the main deliverable. The MVP path (base64 RGBA tiles inside JSON
   text frames) is replaced: tiles are now **binary WebSocket frames** (10-byte
   header + payload, see `src/protocol.rs`) carrying **PNG-compressed RGB**;
   control messages (`resize`/`error`) stay JSON text. Measured live against
   the dev RDP target: the initial
   full-screen paint went from ~31.4 MB (base64-JSON equivalent) to ~3.1 MB on
   the wire — **10x**. Per-session byte totals are logged on disconnect
   (`ws: outbound totals: …`) for measuring in the field.
2. **Server-side VNC session — (done, see docs/vnc.md).** A simple
   Rust RFB client alongside the RDP engine (`src/vnc.rs`) — Guacamole-style:
   protocol 3.8, security None/VncAuth, the raw baseline encoding only, no
   per-implementation workarounds. Both engines feed the same server→browser
   protocol behind the common `Session` seam (`src/session.rs`). The
   follow-ups are planned, not implemented: the full-screen canvas (phase 3,
   common to all protocols) and TigerVNC-style dynamic resize (phase 4).
3. **TOML config, like remotex — (done).** CLI/env-centric config replaced
   with a TOML file in remotex's shape (`[server]` block, `[[targets]]`
   profiles with per-target protocol/host/port/credentials — see
   `../remotex/remotex.example.toml`). Credentials stay server-side.
   Delivered as migration step 1 below.

Explicitly **out**:

- Clipboard support — **not planned yet**, deliberately absent from the phase
  list below: a different approach may be considered instead of porting
  remotex's clipboard sync.
- Soft keyboard mapping and the fancy floating UI (三) from remotex — later,
  phase 7 below.
- Session management (resume, takeover) — the server-side architecture
  *enables* it, but building it is a later phase (6).
- Multi-target UI (config may already hold multiple targets; the UI to pick
  them comes later — phase 8).

And permanently out of scope — never planned, in any phase: **multi session**.
This is a single-user program with one active session only, with session
takeover logic like remotex (`../remotex/server/session.ts`): one global
session slot; claiming it while a session is active fails unless forced, and a
forced claim evicts the previous holder (its WebSocket is closed with a
"Session taken over" code, hard-terminated shortly after). Resume, takeover,
and multi-target all mean re-attaching to or choosing *the* one session/target
— never concurrent sessions, session sharing, or a broker.

## Later phases (sketch)

**Done:** phase 1 (the MVP, docs/phase1-mvp.md), phase 2 (this document —
transport, the VNC engine baseline, TOML config), and phase 3 (full-screen
canvas). Everything below phase 3 is **not started**, in planned order:

- **Phase 3 — full-screen canvas, like remotex — (done).** Common to **all
  protocols** — a frontend behavior, not a VNC feature: the canvas fills the
  browser viewport and renders the remote desktop at 1:1 pixels (viewport ×
  `devicePixelRatio`, no scaling blur). When the remote desktop doesn't match
  the viewport — because the protocol or server can't resize — the canvas
  simply overflows and shows scrollbars, exactly like remotex. No
  letterboxing, no scaling. As built: the canvas backing store stays at the
  remote pixel size and its CSS size is remote ÷ `devicePixelRatio`; the
  screen container is an `overflow: auto` scroller; a re-armed `matchMedia`
  listener re-derives the CSS size when `devicePixelRatio` changes (monitor
  moves, browser zoom). The header/status chrome was replaced by a centered
  status overlay shown only until the desktop streams.
- **Phase 4 — TigerVNC-style dynamic resize (docs/vnc.md):** where the server
  supports it, drive the size it renders from the browser (`SetDesktopSize`)
  so the phase-3 scrollbars disappear; servers without it keep the fixed
  connect-time size and the scrollbars.
- **Phase 5 — frontend integration:** port remotex's frontend shell
  (connection flow, base input handling) onto the uniform protocol. With
  decode server-side there is **one renderer** — the RFB decoder,
  `zrleDecoder`, and the rest of the browser-side engine do not come along.
  The soft keyboard and the floating UI move later (phase 7); clipboard is
  not planned (see "Explicitly out" above).
- **Phase 6 — session management:** detach/reattach and remotex-style takeover
  of the single session slot (force-claim evicts the previous browser) —
  backed by the server-owned framebuffer.
- **Phase 7 — soft keyboard + the floating UI:** port remotex's soft keyboard
  mapping/panel and the fancy floating chrome (the draggable 三 button that
  opens the toolbar).
- **Phase 8 — multi-target support:** target picker over the `[[targets]]`
  config (still one active session at a time).
- **Phase 9 — the rename:** when the project is ready to replace the old
  one, rename the GitHub repo to **remotex-v2** and the binary to **remotex**,
  replacing the original. Not done now; documented here so it isn't forgotten.

## Backend seam

Unchanged in spirit from the earlier proposal, but now both impls decode
server-side and emit the same messages. As built (`src/session.rs`), the seam
is a shared engine signature rather than the trait sketched earlier — two
engines and no dynamic dispatch make a `match` simpler, and IronRDP's
non-`Send` futures fit a plain function better than a trait object:

```
// Both emit the uniform ServerMsg stream (tiles/resize/error);
// input flows back as ClientMsg.
rdp::run(config, input_rx, frame_tx)   // IronRDP, server-side decode
vnc::run(config, input_rx, frame_tx)   // Rust RFB client, raw encoding only

session::spawn(target, …)              // dispatches on target.protocol
```

The earlier idea of gating the engines behind cargo features (so a VNC-only
build skips the IronRDP tree) was dropped: this is a single personal binary
that always ships both. The session kind and target come from the TOML target
profile selected at connect time.

## Migration sketch (rough order)

1. **(done)** TOML config in remotex's shape (`[server]` + `[[targets]]`),
   replacing the CLI/env config entirely — no env vars, no `.env` (env files
   shadowing the environment caused subtle bugs under bun).
2. **(done)** Transport efficiency: tiles moved to binary WS frames with
   PNG-compressed payloads;
   ~10x smaller than the base64-JSON baseline on a real full-screen paint.
   Session byte totals are logged so it can also be measured over a real
   constrained link. End-to-end coverage: `tests/rdp_tiles_e2e.rs` runs a
   dummy xrdp container (podman/docker) and validates the wire format against
   a real RDP session.
3. **(done)** The `Session` seam: both engines expose the same
   `run(config, input_rx, frame_tx)` contract, dispatched by target protocol
   in `session::spawn` (`src/session.rs`). A `match` over two engines
   replaced the trait sketched below — no dynamic dispatch is needed, and
   IronRDP's non-`Send` futures fit a plain signature better than a trait
   object.
4. **(done)** `vnc::run`: a minimal Rust RFB client (3.8 handshake, VncAuth,
   raw encoding, pointer/key input), feeding the shared protocol — see
   docs/vnc.md. End-to-end coverage: `tests/vnc_tiles_e2e.rs` runs a dummy
   TigerVNC container (podman/docker) and validates a full-desktop paint
   through the real server.
5. Verify against real targets — including macOS Screen Sharing, the case that
   motivated dropping the clever-encoding path.
6. **(done)** Phase 3: full-screen canvas at 1:1 device pixels, overflow
   scrolls (see the phase list above).
7. Later phases (4–9 above): dynamic resize, frontend integration, session
   management, soft keyboard + floating UI, multi-target UI, and finally the
   remotex-v2 rename.
