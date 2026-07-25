# `rxa` — a purpose-built macOS agent for remotex

Status: **built and running.** This is the design record: why the thing exists,
what was decided and why, what the machine disagreed with, and what was left
undone. How it works is
[`architecture.md`](architecture.md#rxa-srcrxars); how to install and sign it is
[`packaging/macos/README.md`](../packaging/macos/README.md).

## Why it exists

remotex used to reach the Mac over **macOS Screen Sharing**, Apple's built-in
VNC server, through the RFB client in `src/vnc.rs`. That was an ongoing pain
point:

- It is flaky — the session drops and does not come back cleanly.
- A disconnect forces you to **log in again**. The credential prompt belongs to
  Apple's server, not to remotex, so there was nothing to fix on our side of the
  RFB connection.
- It ignores RFB `SetDesktopSize`, which is why a `displaymode.swift` helper had
  to exist at all.
- It never composites a cursor into the framebuffer, which is the whole reason
  for the Cursor pseudo-encoding path (`-239`) in `src/vnc.rs`.

RealVNC is stable and does not re-prompt, but its free tier has no LAN
direct-connect.

**The fix:** stop speaking VNC to the Mac. Own both ends. Because the
pre-shared key lives in remotex's config, a reconnect is a two-message
cryptographic handshake with **no interactive login, ever** — which is precisely
the property that was missing.

## Decisions

| Decision | Choice |
|---|---|
| Where the agent lives | A cargo **workspace** in this repo, not a sibling repo, so the wire protocol cannot drift between the two sides |
| Pixel path | **Pass-through tiles**, adaptive PNG/JPEG per tile; the gateway relays encoded bytes with no decode and no re-encode |
| Transport | TCP + `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` via `snow` |
| v1 scope | Screen + keyboard/mouse + cursor shapes. Out: dynamic resize, clipboard, audio |

The protocol is **`rxa`** (remote**X** **a**gent); `protocol = "rxa"` in a
`[[targets]]` profile, default port 52381.

## Where the mechanics are documented

The protocol, the capture and encode pipeline, input injection, the two TCC
grants and the reconnect rule are all described in
[`architecture.md`](architecture.md#rxa-srcrxars), which is kept current with
the code. They are not repeated here — a second copy drifts, and this document
is about the reasoning rather than the mechanism.

What follows is the part that only makes sense as history.

## What changed on contact with the machine

The plan survived largely intact. Six things went differently, each a discovery
rather than a preference:

1. **Packaging.** The plan called for an install script laying a plist into
   `~/Library/LaunchAgents` and running `launchctl`. `SMAppService` does it
   better: the bundle carries its own LaunchAgent plist, registers itself on
   first launch, and appears in System Settings → General → Login Items.
   Installing is dragging the app in and opening it once. This raised the floor
   to macOS 14.
2. **The Swift bridge bites.** `screencapturekit` was the right pick, but its
   bridge is built with `swift build --triple arm64-apple-macosx` — no OS
   version. Swift then links the *back-deployment* concurrency runtime as
   `@rpath/libswift_Concurrency.dylib` with no matching runpath, so the agent
   builds clean and dies at startup. Fixed at the root with
   `MACOSX_DEPLOYMENT_TARGET` in `.cargo/config.toml`, not with a runpath.
3. **`SCFrameStatus::Started` carries pixels, and the status is often absent.**
   Accepting only `Complete` meant a static screen produced no tiles at all.
   Worse, and hiding behind it: `frame_status()` reads `None` for every frame on
   macOS 26, so a gate requiring a positive content status rejected everything.
   A missing status must not mean "no pixels" — only a positively-empty status
   may skip a frame.
4. **Cursor representations lie about scale.** Taking the largest
   `NSBitmapImageRep` is wrong: a system cursor can carry a vector-backed rep at
   an arbitrary resolution. The I-beam reports a 14×20 point size with a 280×400
   rep available, so "largest" produced a cursor 20× oversized with its hotspot
   scaled to match. The right pick is the rep closest to *point size × the
   capture's backing scale* — the capture's, not the main display's.
5. **`clamp_u16` moved too.** The plan said to extract only `host_port`; the rxa
   engine needs coordinate clamping as well. Both live in `src/engine.rs`, and
   the duplication did not grow.
6. **Ad-hoc signing is not enough.** The plan assumed `codesign -s -` with a
   stable bundle identifier would keep the TCC grants across rebuilds. It does
   not — an ad-hoc signature has no stable identity, so every rebuild
   re-prompts. A real Developer ID identity is what makes the grants persist.

One thing the plan **omitted entirely**: a menu bar item. It specified a
headless background agent, and that is exactly what shipped — with the result
that nothing on the Mac indicated the agent was running, nothing indicated when
somebody was watching the screen, and stopping it meant `--unregister` plus
`pkill` from a terminal. For software whose whole job is to let a remote machine
see and drive this one, an invisible process with no off switch is the wrong
default. `menubar.rs` adds the status item, and it forced the LaunchAgent's
`KeepAlive` from `true` to `SuccessfulExit: false` — under a plain `true`
launchd resurrects the agent seconds after Quit and the menu item is a lie.

## How the risks turned out

| Risk the plan named | Outcome |
|---|---|
| ScreenCaptureKit binding maturity | The dirty rects were reachable, as hoped. The Swift bridge caused the trouble instead, in a way nobody predicted |
| TCC grants across rebuilds | Real, and worse than expected — ad-hoc signing does not solve it (see above) |
| Retina encode cost | Still open. Everything here was built and verified against a 1280×800 1× display, so a Retina desktop is unmeasured. The fallback ladder is in [`roadmap.md`](roadmap.md) |
| `panic = "abort"` applies to the agent | Still true. An unwrap in a dispatch-queue callback kills the agent. `KeepAlive` restarts it, but the capture path stays disciplined about errors |
| No login-window support | Confirmed exactly as written, and it is the one limitation a user is likely to notice. Both TCC grants need a window server connection, which exists only in the user's GUI session, so a Mac with nobody logged in has nothing for the gateway to reach. What it would take to change that — and why it is not planned — is in [`roadmap.md`](roadmap.md) |

Two things the plan asked for were **not built**, both deliberately and both
recorded with their reasons in [`roadmap.md`](roadmap.md): the post-disconnect
capture-stream linger, and a multi-threaded encoder pool.
