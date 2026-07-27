# macOS agent architecture

`remotex-agent` is the optional macOS endpoint for `protocol = "rxa"`, offered
as a dedicated-agent alternative to connecting the gateway directly to macOS
Screen Sharing over VNC. Its PSK authenticates reconnects directly instead of
returning to Screen Sharing's login gate. It captures the logged-in user's
display, encodes changed regions, and accepts input from the remotex gateway.
The agent and gateway share the protocol crate so their wire types and key
handling stay in sync.

Installation and operation are documented in
[`packaging/macos/README.md`](../packaging/macos/README.md).

## Components

```
remotex gateway                         macOS
src/rxa.rs                              crates/rxa-agent
    │                                      │
    └──── Noise-encrypted rxa over TCP ────┤
                                           ├─ ScreenCaptureKit capture
                                           ├─ PNG/JPEG tile encoder
                                           ├─ Core Graphics input injection
                                           └─ menu bar UI and SMAppService

crates/rxa-proto: PSKs, handshake, framing, messages, and key mapping
```

- `rxa-proto` is cross-platform and contains everything both endpoints must
  interpret identically.
- `rxa-agent` is macOS-only. ScreenCaptureKit supplies frames and dirty
  rectangles; Core Graphics supplies cursor data and input events.
- `src/rxa.rs` connects the agent to the common browser/session interface. It
  relays encoded tiles without decoding and reconnects established sessions
  after transient network failures.

## Transport

The transport is TCP on port 52381 by default, protected with
`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`. The protocol version is included in
the Noise prologue. A generated `rxa...` pre-shared key authenticates both
endpoints and includes a checksum to catch transcription errors.

Noise transport frames carry length-prefixed `rxa-proto` messages:

- agent to gateway: desktop size, the display list and which display is being
  shared, PNG/JPEG tiles, cursor shape, pasteboard text (on request or when the
  watched pasteboard changes), and heartbeat pongs;
- gateway to agent: mouse, wheel, and keyboard input, session control, a display
  selection, clipboard read requests, writes and the watch toggle, and heartbeat
  pings.

The gateway translates these into the same browser protocol used by RDP and
VNC. It passes tile payloads through byte-for-byte. RXA ping/pong independently
checks the gateway-agent link; browser liveness is handled by the gateway's
shared WebSocket/session layer.

## Which display

One at a time, and the client's to choose. The agent reports every display it
could share — `AgentMsg::Displays`, sent beside `Hello`, on `Attach`, after a
switch, and whenever a two-second poll finds the set changed — and a client
answers with `GatewayMsg::SelectDisplay` naming one by `CGDirectDisplayID`.
Positions in a list are deliberately not identities: attaching or unplugging a
screen renumbers everything after it.

A session starts on one of the Mac's **own** screens — its main display, or, if
macOS has made the display the agent created the main one, the first real screen
it lists. Never on the agent's own display, which is the point of enabling one
being separate from sharing it: creating it puts it on the menu, and sharing it
is a choice someone makes, every session, on purpose. (Only a Mac whose *only*
display is ours starts there, having nothing else to offer.)

The choice lasts as long as the session and nothing about it is written to the
config: the person at the far end is picking a screen for as long as they are
looking at it, and the next connection should start from the same place rather
than from wherever the last one wandered off to.

Switching restarts the capture stream, which is why the agent reuses the
teardown-and-restart its capture-failure path already had — including putting the
old display back if the new one cannot be captured, because the stream was torn
down to make room and leaving it down would freeze the desktop. The new size goes
out before any tile drawn at it, and the injector's scale and origin are
re-derived before the first click on the new display arrives.

Clients hold no display state of their own. The checkmark follows the `active` in
the agent's report, so a selection that failed leaves the menu agreeing with what
is on screen. And this is the *only* display decision a client makes: see
Resolution below for the one it does not.

## Capture and encoding

The agent captures one selected physical display. ScreenCaptureKit frame
metadata identifies changed regions; those regions are divided into tiles and
encoded in order. PNG is used for flat or lossless-friendly content and JPEG
for image-like content.

Encoding is intentionally ordered on one worker. Parallel tile completion
could allow an older tile to overwrite a newer update to the same region.
Desktop-size changes are ordered with tiles for the same reason.

Cursor shapes are read separately from the framebuffer and sent with their
hotspot. The representation closest to the capture display's backing scale is
used.

## Resolution

The Mac's, and only the Mac's — and the contrast with the section above is the
whole point. *Which* screen to look at is a question about the person looking, so
a client answers it. *What resolution* that screen runs at is a question about
the machine, so the machine answers it.

There is no message on this wire that asks it to
change resolution, and `resize = true` on an `rxa` target is a config error.
Whoever is using that machine sets the mode where every other mode is set — in
System Settings > Displays — and the agent finds out the same way for every kind
of display: `Capture::follow_display` re-measures on the cursor tick, resizes the
capture surface, and the new size travels as `AgentMsg::DisplaySize` ordered with
the tiles it applies to.

That includes the display the agent creates for itself, which is why it needs no
mechanism of its own — see below. Selecting a different display is not a
resolution change either: the size that follows is the size that display was
already at.

Some displays change size with nobody touching System Settings at all. A UTM
guest's default screen is the host's to size: Apple Virtualization's
`automaticallyReconfiguresDisplay` follows the VM window, so dragging that window
pushes an arbitrary new size into the guest. The agent cannot distinguish this
from a mode switch and does not try — it is the same poll and the same
`DisplaySize`. It does have to survive it: a host-driven reconfigure does not
resize the capture stream, it *kills* it (ScreenCaptureKit loses the display the
filter names), so the session restarts capture under a backoff rather than
treating it as the end.

The scale is re-sent with every size, because a mode switch can change it: the
same panel has HiDPI and 1x modes, and 1920x1080 HiDPI and 3840x2160 at 1x are
the same pixel count presented at different sizes.

## A display of our own

Ticking **Add a private 2x display** in the settings dialog gives the Mac an
extra display, made with the private `CGVirtualDisplay` API — a 2x desktop that
nobody is sitting in front of. It is created once at startup and released when
the agent exits; the process owns it, so a crash cannot leave one behind. A
failure to create it costs the extra display and nothing else, with a line in the
log, because a macOS release that takes the private API away should cost the
sharpness rather than the agent.

An **addition**, not a replacement, which is all `CGVirtualDisplay` was ever able
to be: it adds a monitor beside the ones already attached. The Mac's own screens
stay shareable and the new one simply joins the list a client picks from. This
setting therefore decides only whether that display exists — never which display
is shared, which is a per-session choice made from the viewer or the browser, and
which no session makes by default.

That last part needs saying because macOS may not agree about which display is
which: on the test VM it made the created display the *main* one and arranged the
Mac's own panel to its left. So a session starting on "the main display" started
on ours, and Which display above says what it does instead.

Created once, and then it belongs to macOS. It appears in System Settings >
Displays with the mode list macOS derives from the descriptor — a HiDPI entry and
a `(low resolution)` one at each size — so it is resized exactly where every
other display on that Mac is resized. The agent never applies a second
configuration to it.

The API reports success for configurations that do not work, so
`virtualdisplay.rs` checks rather than trusts. The mode is listed at the
**point** size with `hiDPI = 1`, which is what makes macOS supply twice the
pixels; listing it at the pixel size yields the same point size with no extra
pixels. HiDPI then engages only while pixel density — mode pixels over
`sizeInMillimeters` — stays inside roughly 149–264 dpi, measured on macOS
26.5.2. The display is therefore created at the top of that window, which is
also where `maxPixels` sits, so the configured size is the largest mode that can
be 2x and every smaller one macOS offers has density to spare. Outside that
window macOS silently produces a 1x desktop at *twice* the requested point size,
which is what the check after `applySettings:` is looking for.

Three readings misreport such a display, and each has a workaround here.
`CGDisplayCopyDisplayMode` returns NULL and `SCContentFilter.pointPixelScale`
reads 1.00 even at 2x, so neither the geometry nor the backing scale can be read
back: the size comes from `CGDisplayBounds` and the scale is *derived* from it by
`capture::owned_scale`. `maxPixels` is twice the created size and cannot change,
so a mode at or under that size is 2x and anything larger provably is not — which
is what keeps a `(low resolution)` pick from being captured as a surface four
times the size it should be. Third, `CGDisplayBounds` reports the requested size
even for a display the WindowServer refuses to bring online, so creation also
checks `CGDisplayIsOnline` and `CGDisplayIsActive`. That offline state is
remembered against the display's identity and survives a reboot, so it retries
once with a serial macOS has not seen.

What this costs in practice — the ways macOS can leave such a display unusable —
is in [`known-issues.md`](known-issues.md).

## Input

Browser DOM key codes are mapped to macOS virtual key codes in `rxa-proto`.
Mouse coordinates are clamped to the captured display and injected with Core
Graphics. The agent requires Accessibility permission for input and Screen
Recording permission for capture.

## Lifecycle

The app registers its embedded LaunchAgent with `SMAppService` and runs in the
logged-in user's GUI session. Its menu bar item exposes status, settings, the
PSK, permission shortcuts, logs, and the login-item toggle.

Only one gateway may be connected. A new authenticated connection replaces the
old one. The shared browser heartbeat ends the engine under the same policy as
RDP and VNC: a missing pong expires it after about 60 seconds, while an orderly
WebSocket close allows 60 seconds for reattachment. Ending the RXA engine closes
the agent connection, stops capture, and changes the menu from "Sharing this
screen" to "No gateway connected."

Separately, the gateway sends the agent an RXA ping every five seconds. The
agent answers with an RXA pong; a silent agent link is reconnected after its
15-second deadline. The agent has a longer idle timeout as a final guard against
a silent gateway.

When an established agent link drops while the browser remains live, the
gateway reconnects with capped backoff, for up to 30 seconds. On recovery it
requests a full repaint and reports a resize only if the display dimensions
changed. Input accumulated during an outage is discarded. An initial connection
or authentication failure is reported immediately.

Past 30 seconds the browser is told and lands on the picker with the reason. The
silent reconnect is for the link that comes back — a Wi-Fi roam, a settings save
restarting the agent — and a Mac that was switched off or had its lid closed does
not. Retrying it indefinitely left the browser holding a frozen desktop with
nothing to say. The window is measured per outage, so a link that comes back
resets it.

Saving settings restarts the agent so address, display, and key changes take
effect together. A deliberate quit stays stopped; crashes are restarted by
launchd.

## Constraints

- The agent mirrors one whole display at a time and never follows client viewport
  reports. A client chooses *which* display; it never changes any display's
  resolution — see Which display and Resolution above.
- It runs only in a logged-in GUI session. It does not support the macOS login
  window or an unattended service mode.
- Screen Recording and Accessibility grants are tied to the app's signing
  identity, so builds must not alternate between identities. An ad-hoc signature
  differs on every build, and the grants then neither carry over nor re-prompt:
  System Settings keeps the stale path-matched entry while the system refuses
  the app behind it, so each install needs both entries removed and re-added by
  hand. Mixing sources — the ad-hoc GitHub release over a locally signed build,
  or the reverse — does the same. `packaging/macos/build-agent-app.sh` therefore
  signs with a keychain identity by default and treats ad-hoc as a last resort.
- The clipboard has no change notification to subscribe to. AppKit's
  `NSPasteboard` posts none — unlike iOS `UIPasteboard` — so the agent polls
  `changeCount`, which is a counter rather than a pasteboard access, and reads
  the *contents* only when it moves. That keeps content reads to one per copy.
  Reads happen only while the gateway has enabled the watch, which it does only
  for a target with `clipboard = true`; otherwise the agent never looks.
- macOS 15.4+ governs those content reads. The general pasteboard asks the user
  by default, and only after the first alert does the app appear in System
  Settings › Privacy & Security › Paste from Other Apps, where "Always Allow"
  makes sync silent. The agent reads `NSPasteboard.accessBehavior` and reports
  it in the menu bar, since the property is read-only and the fix is the user's
  to apply. This is the sole reason the deployment target is 15.4.
- Audio is not part of the current protocol.
