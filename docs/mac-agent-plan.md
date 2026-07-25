# `rxa` — a purpose-built macOS agent for remotex

Status: **built and running.** This is the design record: why the thing exists,
what was decided and why, what the machine disagreed with, and what was left
undone. It is not a specification — the code is. Where a detail matters at the
byte level, the source is cited rather than copied, because a copy drifts.

For the system as built see [`architecture.md`](architecture.md#rxa-srcrxars)
and [`packaging/macos/README.md`](../packaging/macos/README.md).

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

## The shared crate

`crates/rxa-proto` is pure Rust with no platform dependencies, and it exists in
order to be **unit-tested on Linux** — the agent crate never compiles there, so
anything testable had to live outside it. It carries the PSK format, the
handshake, the framing, the message set, and the DOM-code → macOS-keycode table.

Three choices in it are worth keeping a record of:

**The PSK carries a checksum.** `rxa` + base64url(32 random bytes ‖
CRC16-CCITT-FALSE), 49 characters, matching the house format used elsewhere.
The CRC turns a transcription typo into "invalid psk: checksum" at config-parse
time instead of an opaque handshake failure once the gateway starts dialling.

**The Noise prologue is `b"rxa/1"`**, which binds the protocol version into the
handshake transcript. A version mismatch fails cleanly at the handshake rather
than producing confused garbage several messages later. The PSK alone provides
mutual authentication: no certificates, no CA, no pinning, nothing that expires.

**Framing treats the Noise transport as a byte stream.** A Noise transport
message caps at 65535 bytes and a full-screen keyframe is far larger, so rather
than inventing a chunking flag, `frame.rs` splits transparently into ≤65519-byte
Noise messages and application framing sits on top as plain `u32 LE length + u8
type + body`. Chunking stays out of the message definitions entirely, and the
whole thing is testable against an in-memory duplex.

One thing this shape forced: the read and write halves each need their own nonce
counter, so `frame.rs` holds a `snow::StatelessTransportState` behind an `Arc`
rather than the stateful transport. With the stateful one, a reader blocked
waiting for a tile would block the writer trying to send input.

## The agent

`crates/rxa-agent`, macOS-only, excluded from the workspace's `default-members`
so a bare `cargo build`/`clippy`/`test` on Linux never reaches for it.

**Capture** is ScreenCaptureKit via the `screencapturekit` crate, chosen over
`objc2-screen-capture-kit` for one reason: it exposes `dirtyRects` directly.
Only dirty regions are encoded and sent, and the plan named reaching them as the
single biggest risk in the project. `SCStreamConfiguration` runs BGRA, 30 fps,
small `queueDepth`, no audio, and `showsCursor = false`.

Two pre-empted bugs, both of which would have been real:

- **Stride.** `CVPixelBuffer`'s `bytesPerRow` is not `width * 4`. Rows are read
  at the reported stride; getting this wrong shears the image.
- **Backing scale.** A Retina display captures at pixel dimensions that differ
  from the point dimensions `CGEventPost` wants. Both are kept and converted at
  the input boundary.

The delegate callback arrives on an arbitrary dispatch queue and does the
minimum: extract the dirty pixels, hand them on, never block. Blocking
ScreenCaptureKit's queue stalls capture itself.

**Encoding** splits each dirty rect at `STRIP_ROWS = 64` so a full repaint does
not become one enormous message, then picks a codec per tile: PNG
(`Compression::Fast`, the same settings the gateway already used) for flat and
text content, JPEG (`jpeg-encoder` with `simd`, pure Rust, no C dependency) for
photographic. The classifier samples a strided subset and counts distinct
colours quantised to 5 bits per channel — few colours means UI, many means
photo. On a real desktop this lands about where you would expect: text and
chrome as PNG, wallpaper gradients as JPEG.

When the link is slow the sink **coalesces rather than queues**: it drops the
frame and sets the full-repaint flag, so falling behind becomes one later,
coarser repaint instead of a flood of stale tiles.

**Input** is `CGEvent*` + `CGEventPost`. Keys go through
`rxa_proto::keymap::mac_keycode` with `CGEventFlags` tracked from the modifiers
the browser reports; remotex sends `caps` as an authoritative flag on every key
event, so the agent never has to infer lock state. Pointer coordinates arrive as
framebuffer pixels and must become global display points — divide by the
capture's backing scale, offset by the display origin. That conversion is the
most likely "clicks land in the wrong place" bug in the whole agent, so it is a
pure function with tests at 1×, 2× and offset corners.

**Cursor shapes** ride the existing `cursor` control channel, so the frontend's
`paintCursor` — built for VNC — needed no changes at all. The shape is cached
and resent on attach; otherwise a browser connecting later would have no pointer
until the shape happened to change.

**Permissions** are the part to read before writing any code. Two separate TCC
grants, both one-time and both user-visible: **Screen Recording** for `SCStream`
and **Accessibility** for `CGEventPost`. Three consequences shaped the design:

1. The agent must run in the user's GUI (Aqua) session — a LaunchAgent, not a
   LaunchDaemon. A daemon has no window server connection and both capture and
   injection fail outright.
2. Therefore it is **not running at the login window** and cannot be. See
   [the last section](#running-at-the-login-window-like-realvnc).
3. Grants are keyed to the signed code identity, so signing is not optional.

Neither grant can be obtained implicitly by using the API. `SCShareableContent`
fails with something that reads like a refusal but also happens when the
question was never asked, and `CGEventPost` never fails at all — it silently
does nothing. The agent therefore calls `CGRequestScreenCaptureAccess` and
`AXIsProcessTrustedWithOptions` explicitly at startup, which is what puts it in
the two Settings lists in the first place.

## The gateway side

Small and contained, which was the payoff of the pass-through design.

- `src/protocol.rs` — `Tile` gained a `format: u8` field and a
  `Tile::encoded(...)` constructor that takes already-encoded bytes. RDP and VNC
  are unaffected.
- `src/rxa.rs` — the engine, same seam as every other one.
- `src/config.rs` — `Protocol::Rxa`, a `psk` field validated at parse time, and
  `resize = true` rejected on an rxa target.
- `src/engine.rs` — `host_port` and `clamp_u16`, extracted rather than
  triplicated. Explicitly **not** a `trait Engine` refactor.
- The frontend needed two lines: accept format `2` and pass the tile's mime type
  to the `Blob`. `createImageBitmap` decodes JPEG natively.

**The one place this engine deliberately differs from RDP and VNC** is
reconnect. An initial connect failure surfaces as an error and returns, exactly
like the others — a wrong host or a wrong key should be visible immediately. But
an *established* session that drops retries silently with capped backoff (1 s →
15 s) forever, re-sending the size and requesting a full repaint each time. The
browser sees frames pause and resume; it never bounces back to the picker and
there is never a credential prompt. That rule is the concrete expression of "a
disconnect doesn't force you to log in again", which is the entire point of the
project.

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

## Deliberately not built

- **The post-disconnect stream linger.** It only pays for outages shorter than
  the gateway's own 1 s minimum backoff, and restarting the stream costs about
  as much. Noted in `session.rs`.
- **A multi-threaded encoder pool.** A pool lets two frames' tiles finish out of
  order, and the same region is commonly dirty in consecutive frames, so an
  older tile could land on top of a newer one and leave stale pixels until
  something else redraws them. Ordering is worth more than the parallelism until
  measurement says otherwise.

## How the risks turned out

| Risk | Outcome |
|---|---|
| ScreenCaptureKit binding maturity | The dirty rects were reachable, as hoped. The Swift bridge caused the trouble instead, in a way nobody predicted |
| TCC grants across rebuilds | Real, and worse than expected — ad-hoc signing does not solve it (see above) |
| Retina encode cost | **Unmeasured.** Everything here was built and verified against a 1280×800 1× display. The fallback ladder if it does not keep up, in order: downscale in `SCStreamConfiguration` → coarser tiles → hardware H.264 via VideoToolbox with WebCodecs in the browser. The last one is deliberately a last resort; it would add an in-browser decoder path, which `architecture.md` lists as a tenet to avoid |
| `panic = "abort"` applies to the agent | Still true. An unwrap in a dispatch-queue callback kills the agent. `KeepAlive` restarts it, but the capture path stays disciplined about errors |
| No login-window support | Confirmed, and unchanged. See below |

## Running at the login window, like RealVNC

The agent needs a logged-in user. If nobody is logged in on the Mac, there is
nothing for the gateway to reach. This is a property of the design, and getting
out of it is a bigger job than it looks.

Two states get conflated, and only one of them is hard:

- **Logged in, screen locked.** The agent is already running in that session.
  Whether ScreenCaptureKit still yields pixels over the lock screen is
  **untested here** and worth ten minutes to find out, because if it does, this
  case already works and the remote can type the password to unlock.
- **Logged out, sitting at the login window.** This is the RealVNC-style
  capability, and it needs real work.

What that second one takes, roughly in order of increasing difficulty:

1. **A second launchd job**, with `LimitLoadToSessionType = LoginWindow`. That
   key is how a process gets into the loginwindow context; it is what Apple's
   own accessibility agents use to run at the login screen. It cannot be
   expressed through `SMAppService`, so the plist has to be installed into
   `/Library/LaunchAgents` by something running as root. That means a signed and
   notarized installer package, and it means giving up the entire "drag it in
   and open it once, uninstall by dragging to the Trash" story that the current
   packaging is built around.
2. **Running as root, not as you.** The login-window job has no user. The config
   and the pre-shared key would have to move out of
   `~/Library/Application Support` to a system path with its own permissions
   story.
3. **Handing the session over.** When somebody logs in, the login-window job is
   unloaded and the Aqua one is loaded; at logout it reverses. Two processes
   want the same port, so either the listener migrates between them or a
   privileged daemon owns the socket and brokers to whichever agent currently
   has a window server. The gateway's silent reconnect would hide the gap
   nicely, which is a genuine advantage of the design here.
4. **TCC, which is the real wall.** Both grants are per-user, and at the login
   window there is no user — the system TCC database governs instead, it is
   SIP-protected, and it cannot be edited by hand. The supported way to
   pre-authorize it is an MDM Privacy Preferences Policy Control payload, which
   means enrolling the Mac in an MDM. I have **not verified** whether Screen
   Recording specifically can be allowed that way, as opposed to only denied;
   that question should be settled before any of the work above is started,
   because if the answer is no, none of the rest matters.
5. **Whether ScreenCaptureKit works there at all.** Also unverified. Apple's own
   `screensharingd` predates SCK and uses different plumbing, which is not
   evidence either way but is not encouraging.

And a floor none of it gets under: **with FileVault on, nothing runs until
someone unlocks the disk at the pre-boot screen.** The volume is not mounted, so
there is no agent, no launchd job and no login window. "No login needed" already
has a hard limit on any Mac with FileVault enabled, including this one.

**The cheap alternative,** which gets most of the benefit for none of the work:
turn on automatic login. With FileVault enabled the pre-boot unlock already
authenticates the user, so automatic login brings the Aqua session straight up
after it — the agent starts with the session and the Mac is reachable from boot.
What remains uncovered is only the case where someone deliberately logs out,
which for a single-user personal machine is a rare thing to design a privileged
system service around.
