# Phase 2 — Consolidating VNC (remotex) and RDP

> **Status: decided, not yet implemented.** This supersedes the earlier
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
   header + payload, see `src/protocol.rs`) carrying **PNG-compressed RGB**
   (raw RGB when smaller, e.g. tiny tiles); control messages (`resize`/`error`)
   stay JSON text. Measured live against the dev RDP target: the initial
   full-screen paint went from ~31.4 MB (base64-JSON equivalent) to ~3.1 MB on
   the wire — **10x**. Per-session byte totals are logged on disconnect
   (`ws: outbound totals: …`) for measuring in the field.
2. **Server-side VNC session.** A simple Rust RFB client alongside the RDP
   engine — Guacamole-style: standard/raw baseline encodings only, no
   per-implementation workarounds. Both engines feed the same server→browser
   protocol behind a common `Session` seam.
3. **TOML config, like remotex — (done).** CLI/env-centric config replaced
   with a TOML file in remotex's shape (`[server]` block, `[[targets]]`
   profiles with per-target protocol/host/port/credentials — see
   `../remotex/remotex.example.toml`). Credentials stay server-side.
   Delivered as migration step 1 below.

Explicitly **out** (later phases):

- Clipboard support.
- Soft keyboard mapping.
- The fancy input overlay / frontend chrome from remotex.
- Session management (resume, takeover) — the server-side architecture
  *enables* it, but building it is a later phase.
- Multi-target UI (config may already hold multiple targets; the UI to pick
  them comes later).

And permanently out of scope — never planned, in any phase: **multi session**.
This is a single-user program with one active session only, with session
takeover logic like remotex (`../remotex/server/session.ts`): one global
session slot; claiming it while a session is active fails unless forced, and a
forced claim evicts the previous holder (its WebSocket is closed with a
"Session taken over" code, hard-terminated shortly after). Resume, takeover,
and multi-target all mean re-attaching to or choosing *the* one session/target
— never concurrent sessions, session sharing, or a broker.

## Later phases (sketch)

- **Phase 3+ — frontend integration:** port remotex's frontend shell
  (connection flow, overlay, soft keyboard, clipboard) onto the uniform
  protocol. With decode server-side there is **one renderer** — the RFB
  decoder, `zrleDecoder`, and the rest of the browser-side engine do not come
  along.
- **Session management:** detach/reattach and remotex-style takeover of the
  single session slot (force-claim evicts the previous browser) — backed by
  the server-owned framebuffer.
- **Multi-target support:** target picker over the `[[targets]]` config (still
  one active session at a time).
- **Final phase — the rename:** when the project is ready to replace the old
  one, rename the GitHub repo to **remotex-v2** and the binary to **remotex**,
  replacing the original. Not done now; documented here so it isn't forgotten.

## Backend seam

Unchanged in spirit from the earlier proposal, but now both impls decode
server-side and emit the same messages:

```
trait Session {
    async fn connect(config) -> Result<Self>;
    // Both emit the uniform ServerMsg stream (tiles/resize/error);
    // input flows back as ClientMsg.
}

rdp::Session   // IronRDP, server-side decode            [feature = "rdp"]
vnc::Session   // Rust RFB client, raw/standard          [feature = "vnc"]
               // encodings, server-side decode
```

Cargo features still gate the engines so a VNC-only build does not pull the
heavy IronRDP tree. The session kind and target come from the TOML target
profile selected at connect time.

## Migration sketch (rough order)

1. **(done)** TOML config in remotex's shape (`[server]` + `[[targets]]`),
   replacing the CLI/env config entirely — no env vars, no `.env` (env files
   shadowing the environment caused subtle bugs under bun).
2. **(done)** Transport efficiency: tiles moved to binary WS frames with
   PNG-compressed payloads (raw fallback for tiles where PNG would be larger);
   ~10x smaller than the base64-JSON baseline on a real full-screen paint.
   Session byte totals are logged so it can also be measured over a real
   constrained link.
3. Introduce the `Session` trait; make the current RDP path implement it.
4. Add `vnc::Session`: a minimal Rust RFB client (handshake, VNC auth, raw +
   mandatory baseline encodings, pointer/key input), feeding the shared
   protocol.
5. Verify against real targets — including macOS Screen Sharing, the case that
   motivated dropping the clever-encoding path.
6. Later phases: frontend integration, session management, multi-target UI,
   and finally the remotex-v2 rename.
