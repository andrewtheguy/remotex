# macOS agent architecture

`remotex-agent` is the optional macOS endpoint for `protocol = "rxa"`, offered
as a dedicated-agent alternative to connecting the gateway directly to macOS
Screen Sharing over VNC. Its keypair authenticates reconnects directly instead
of returning to Screen Sharing's login gate. It captures the logged-in user's
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

crates/rxa-proto: identity keys, handshake, framing, messages, key mapping
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
`Noise_KK_25519_ChaChaPoly_BLAKE2s`. The protocol version is included in the
Noise prologue.

Each end holds one long-lived X25519 keypair and pins the other's public key,
the way WireGuard pairs an interface with a peer: the gateway's identity is
`[rxa].private_key` in `remotex.toml` and each target names that Mac's
`agent_public_key`; the agent's identity is `private_key` in its own config and
it names one `gateway_public_key`. `KK` rather than `IK` because each side pins
exactly one peer, so both statics are known before the first byte and
authentication happens entirely inside Noise — there is no list of accepted
gateways and nothing for either endpoint to compare after the fact. Both `es`
and `ss` are consumed in the first message, so a mismatch on either side is
rejected by the agent before it has revealed anything.

Keys are text as `<prefix><base64url of 32 bytes and a CRC16>`, the checksum
catching transcription errors. The prefix carries the role as well as the kind —
`rxgs`/`rxgp` for a gateway's private and public keys, `rxas`/`rxap` for an
agent's — so each of the four config fields accepts one kind and names the other
three. That matters most while pairing, when both public keys are in play at
once and a swap would otherwise surface as an opaque handshake rejection.

Only the two public keys ever move between machines, so both ends display theirs
in full: the agent in its settings dialog and via `remotex-agent --public-key`,
the gateway via `remotex rxa-pubkey` and a line in its startup log.

Noise transport frames carry length-prefixed `rxa-proto` messages:

- agent to gateway: desktop size, the display list and which display is being
  shared, PNG/JPEG tiles, cursor shape, pasteboard text (on request or when the
  watched pasteboard changes), and heartbeat pongs;
- gateway to agent: mouse, wheel, and keyboard input, session control, a display
  selection, a size and a density for a display the agent made, clipboard read
  requests, writes and the watch toggle, and heartbeat pings.

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

A session starts on whichever display the Mac calls main, and the agent does not
argue with that answer — including when the main display is the one the agent
created, which is a thing macOS remembers per arrangement (see A stable identity
below). Second-guessing it would mean overriding a choice made in System
Settings.

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

For a screen somebody is sitting at, that is the whole of it: nothing on this
wire asks a Mac's own panel to change resolution. Whoever is using that machine
sets the mode where every other mode is set — in System Settings > Displays — and
the agent finds out the same way for every kind of display:
`Capture::follow_display` re-measures on the cursor tick, resizes the capture
surface, and the new size travels as `AgentMsg::DisplaySize` ordered with the
tiles it applies to.

The one exception is the display the agent creates for itself, and it is an
exception to the *premise* rather than to the rule: nobody is sitting at that
display, so there is no one on the machine for the question to belong to. A
client may ask for its size — `GatewayMsg::ResizeDisplay`, on the user's request,
gated on the target's `resize` and on that display being the one being shared —
and the answer comes back through the same `follow_display` path as every other
cause. See [Its size follows the client's window, when asked](#its-size-follows-the-clients-window-when-asked).

Selecting a different display is not a resolution change either: the size that
follows is the size that display was already at.

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

An **addition**, not a replacement: `CGVirtualDisplay` adds a monitor beside the
ones already attached. The Mac's own screens stay shareable and the new one simply
joins the list a client picks from. This setting therefore decides only whether
that display exists — never which display is shared, which is a per-session choice
made from the viewer or the browser, and which no session makes by default.

macOS may not agree about which display is which, either: on the test VM it had
our display arranged as the *main* one, so a session started there. That is the
remembered arrangement doing its job — see A stable identity below — and not
something the agent second-guesses.

Created once, and then it belongs to whoever is looking through it. It appears in
System Settings > Displays with the mode list macOS derives from the descriptor —
a HiDPI entry and a `(low resolution)` one at each size — so it can be resized
exactly where every other display on that Mac is resized. Two of its properties
can also be set from here, and the two sections below are why: its density
follows the client automatically, and its size follows the client's window when
the person asks.

The API reports success for configurations that do not work, so
`virtualdisplay.rs` checks rather than trusts. The mode is listed at the
**point** size with `hiDPI = 1`, which is what makes macOS supply twice the
pixels; listing it at the pixel size yields the same point size with no extra
pixels. HiDPI then engages only while pixel density — mode pixels over
`sizeInMillimeters` — stays inside roughly 149–264 dpi, measured on macOS
26.5.2. The display is therefore created at the top of that window, which is
also where `maxPixels` sits, so the initial size is the largest mode that can
ever be 2x and every smaller one macOS offers has density to spare. Outside that
window macOS silently produces a 1x desktop at *twice* the requested point size,
which is what the check after `applySettings:` is looking for.

Two readings misreport such a display, and each has a workaround here.
`SCContentFilter.pointPixelScale` reads 1.00 even at 2x, so the capture size is
set from what was asked for rather than derived from it. And `CGDisplayBounds`
reports the requested size even for a display the WindowServer refuses to bring
online, so creation also checks `CGDisplayIsOnline` and `CGDisplayIsActive`.

`CGDisplayCopyDisplayMode` is *not* one of them, though this document and the code
both said so for a while. It works on these displays and reports the truth —
`3800x2400 px / 1900x1200 pt` at 2x, `1900x1200 px / 1900x1200 pt` for the same
display at 1x — so `capture::owned_display_scale` reads the backing scale from it
exactly as it does for a real screen. The heuristic that stood in for it,
`capture::owned_scale`, is now only the fallback for a macOS that publishes no
mode, because it could not see the case that matters: macOS lists a `(low
resolution)` 1x entry beside each HiDPI one *at the same point size*, and from
points alone those are one number. Whichever entry macOS restored for the identity
decided whether the agent's reported density was right, which is why it looked
random.

## Its density follows the client

One of two things about this display that are not the Mac's to decide, and they
are the two nobody at the Mac is deciding *for*: a display nobody sits in front
of has no right density and no right size of its own, only the right ones for
whoever is looking through it. So both clients report the backing scale of the
screen their window is on — on connect, and again when the window changes screen
— and the agent matches it with `applySettings:` at the display's current point
size.

The point size is deliberately preserved, so the desktop keeps its layout and only
the pixels behind it change; a client connecting from a different screen does not
rearrange anyone's windows. `VirtualDisplay::set_scale` returns early when the
densities already agree, which is the common case, because every apply is a
WindowServer round trip that relays that desktop's windows.

Narrow on purpose, in three ways. It reaches only a display the agent *made* — a
Mac's own panel does not change because someone connected. It changes only the
density, never the resolution; that is the section below. And it changes what
the client *receives*, never how the client draws it: both clients present a
remote at its own point size and let their host rasterize that, so mismatched
densities already look correct — this is what makes them stop costing four times
the framebuffer, or stop being resampled.

## Its size follows the client's window, when asked

The same argument, applied to the other property, with one difference that
decides the whole shape: a reconfigure relays every window on that desktop, and a
guest's display stack can wedge after enough of them ([`known-issues.md`](known-issues.md)).
So this is RDP's shape rather than VNC's — the floating menu's and the viewer's
**Resize to window**, pressed, never a window drag followed forty times.

Measured on the test VM (macOS 26.5.2): `applySettings:` on the live display
honours an arbitrary point size, keeps the same `displayID`, applies in 66–397 ms
and settles 134–580 ms later. `VirtualDisplay::set_size` waits for that settle
before releasing the display lock, and returns early when the display is already
that size, so a second press on a window that did not move costs nothing.

Three gates, and they answer different questions:

- **`resize = true` on the gateway target.** The operator's permission. Accepted
  for `rxa` targets, and it cannot be validated further from the gateway: whether
  a Mac's agent even has a private display lives in that Mac's own config.
- **The shared display is one the agent made.** Both clients read this off the
  `displays` list and enable the control accordingly, as the user picks the
  agent's display or a real screen from the Display menu. They present it
  differently: the viewer greys the menu item, the browser omits its button.
- **The agent agrees.** The only non-racy authority, since it owns the display.
  A request for anything else is dropped in silence — an `AgentMsg::Error` would
  be fatal to the session, and a button that did nothing must never end one.

**Units.** A client's `viewport` is remote *pixels* — its window times the
density the gateway announced — and a display mode is *points*, so the gateway
divides on the way through. It holds the exact scale the client multiplied by,
which makes that an exact inverse; the agent's live density would not be, because
a display publishes no mode to read for tens of milliseconds around a density
change.

**Bounds.** The request is clamped into the envelope creation fixed, per axis, and
never refused — past `maxPixels` there is nothing to refuse with, since
`applySettings:` answers YES and halves the result. Below roughly 57% of the
created width the mode leaves the HiDPI window and comes back 1x; that is applied
rather than clamped away, because 2x is not obtainable there at any size that
could be substituted, and the honest answer is the size that was asked for at the
density it can hold. `capture::mode_scale` then reports the truth.

**Nothing reverts it.** Not on disconnect, not at the next launch, and nothing is
written back to the config file. macOS files the resulting mode against the
display's identity, so a resize asked for from a viewer sticks exactly the way one
made in System Settings sticks — see the next section.

## Its identity, and what macOS remembers against it

The display reports a fixed vendor, product and serial, for the same reason a
monitor does: those are burned into the hardware. macOS files an arrangement
against them — position, mode, whether the display is the primary, and the modes
of the screens beside it — and restores all of it when that identity reappears.
That is what makes a monitor you plug back in come back where you left it, and it
is the behaviour to have rather than one to work around. An identity that changed
between launches would be a new display every time, and would forget the lot.

So the agent takes the arrangement macOS gives it at startup as given. It
measures what it finds and reports that, rather than applying a configuration of
its own accord. Two things follow, and both are only surprising if you expected
the agent to be in charge:

- **A session can start on this display.** It starts on whichever display the Mac
  calls main, and if the arrangement makes ours primary, that is ours. Overriding
  it would mean overruling a System Settings choice.
- **The configured size is an initial size.** `virtual_display_initial_size` is
  what the display is created at the first time a Mac sees it (at least 800x600,
  a floor `config.rs` and `virtualdisplay.rs` share); afterwards the remembered
  mode wins, and editing the setting will not move a display that has already
  been arranged. What the value fixes permanently is the *envelope*, since
  `maxPixels` and `sizeInMillimeters` cannot change after creation.

  A client's Resize to window lands in that same remembered mode, which is the
  intended consequence rather than a leak: a resize asked for from a viewer comes
  back after a restart the way a monitor comes back where you left it. It is also
  the reason to prefer it over the mode list in System Settings, which both the
  config comment and the settings dialog now say. Resizing there is *allowed* —
  it is an ordinary display — but the list offers a `(low resolution)` twin of
  every size, so half of what it presents is 1x at a size that could have been
  2x. Resize to window never picks the twin: it stays inside the envelope and
  re-applies the density the display is in. What it cannot do is hold that
  density below the floor described under Bounds above — a small enough window
  leaves the HiDPI range and comes back 1x there too, at the size that was asked
  for. Neither is recoverable from this process once macOS has remembered it:
  only a new display restores the density, and a new display means a new identity
  and the lost arrangement above.

The one remembered state that is a genuine problem is an arrangement that holds
the identity **offline**. Nothing in the process can clear it, and the agent
reports it rather than minting a new identity to escape it — escaping it would
discard the arrangement, which is the one thing a monitor never does. The fix is
on the Mac, in System Settings, as it would be for a panel that came back dark;
see [`known-issues.md`](known-issues.md).

There is no API for any of this in either direction: nothing on
`CGVirtualDisplayDescriptor` or `CGVirtualDisplaySettings` places a display,
declines the primary role, or clears remembered state.

## Input

Browser DOM key codes are mapped to macOS virtual key codes in `rxa-proto`.
Mouse coordinates are clamped to the captured display and injected with Core
Graphics. The agent requires Accessibility permission for input and Screen
Recording permission for capture.

## Lifecycle

The app registers its embedded LaunchAgent with `SMAppService` and runs in the
logged-in user's GUI session. Its menu bar item exposes status, settings,
permission shortcuts, logs, and the login-item toggle. Neither key is among
them: both live in the settings dialog, shown in full because neither is a
secret — this Mac's public key as a read-only label with a Copy button, the
gateway's as an ordinary field to paste into. The private key is never displayed
anywhere. Replacing it is a Regenerate identity button in that dialog, behind a
confirmation, because the cost is not local: every gateway paired with this Mac
must be given the new public key before it can reach it again.

An agent with no `gateway_public_key` is unpaired: it listens, refuses every
connection, and says so in its menu. That is the state a first launch lands in,
since the agent has to be running before its public key can be read off it.

Only one gateway may be connected. A new connection replaces the old one when it
completes its handshake, not when it is accepted: a peer that cannot prove it
holds the paired key is refused without the running session noticing, so nothing
that can merely reach the port can end a session by opening a socket. Handshakes
run off the accept path and on a 20-second timeout, so one silent peer cannot
hold up the connection behind it either.

The shared browser heartbeat ends the engine under the same policy as
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
