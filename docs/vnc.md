# VNC engine

The server-side VNC session promised by phase 2 (see
[`phase2-consolidation.md`](phase2-consolidation.md), scope item 2), split into
its own two phases.

## Phase 1 — baseline RFB client (implemented)

`src/vnc.rs` is a minimal RFB client (RFC 6143) that sits behind the same
`Session` seam as the RDP engine (`src/session.rs`) and feeds the identical
server→browser protocol: `resize`/`error` as JSON text, screen updates as
binary PNG tiles. The browser cannot tell the protocols apart.

Guacamole-style baseline, no per-implementation workarounds:

- **Protocol 3.8.** Anything announcing at least 3.8 is answered with 3.8
  (macOS Screen Sharing greets with 3.889, RealVNC with 4.x — both accept a
  3.8 client). Older servers are rejected.
- **Security None or classic VncAuth** (DES over the 16-byte challenge with
  the RFB bit-reversed key convention). VncAuth is chosen when the target has
  a password, otherwise None. Apple/RealVNC proprietary types are not spoken.
- **Raw encoding only.** The one encoding every VNC server must support. The
  backend↔VNC hop is LAN (see the phase-2 rationale), so VNC's clever wire
  encodings buy nothing there; the link that matters — backend→browser — is
  optimized by the shared tile transport (PNG-compressed binary frames).
- **Forced pixel format:** 32bpp true-colour BGRX little-endian, repacked
  server-side to RGB and PNG-encoded in the same `STRIP_ROWS` strips as RDP.
- **Input:** pointer events carry the tracked button mask + position (wheel =
  press/release of buttons 4–7); keys map DOM `code` → X11 keysym via
  `keymap::keysym` (US layout, unshifted symbol — the server combines it with
  the modifier state it tracks from Shift/Ctrl/… key events).

Covered by unit tests (handshake pieces, auth vectors cross-checked against
remotex's implementation, input encoding) and an end-to-end test against a
real TigerVNC server in a container (`tests/vnc_tiles_e2e.rs`).

Deliberately out of phase 1: clipboard (`ServerCutText` is drained and
dropped), Bell, and any resize handling — the desktop size is fixed at
connect time.

## Phase 2 — full-screen canvas + dynamic resize (planned, not implemented)

Two related follow-ups, in the spirit of remotex's frontend:

1. **Full-screen canvas, like remotex — common to all protocols.** This is a
   frontend behavior, not a VNC feature: the canvas fills the browser
   viewport and renders the remote desktop at 1:1 pixels (no scaling blur).
   The desired remote size is viewport dimensions × `devicePixelRatio`
   (`../remotex/src/resizeSizing.ts`). When the remote desktop doesn't match
   the viewport — because the protocol or server can't resize — the canvas
   simply overflows and shows scrollbars, exactly like remotex. No
   letterboxing, no scaling.

2. **TigerVNC-style dynamic resize.** Where the server supports it, drive the
   size it renders from the browser so the scrollbars disappear: advertise
   the `ExtendedDesktopSize` pseudo-encoding and send `SetDesktopSize` when
   the viewport changes, so TigerVNC-family servers (Xtigervnc, x0vncserver,
   …) re-render at the new geometry. Servers that don't support it keep the
   fixed connect-time size and fall back to the scrollbars above — acceptable
   per the no-workarounds rule. The RDP engine has the same gap
   (deactivation/reactivation is logged, not handled); the browser-side
   plumbing — a `ClientMsg` for viewport changes and re-`Resize` handling —
   should be designed once for both engines.

Neither is started; both depend on frontend work scheduled with the phase-3
remotex frontend integration (see the later-phases sketch in
`phase2-consolidation.md`).
