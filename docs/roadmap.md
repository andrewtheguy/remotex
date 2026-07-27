# Roadmap

Defects, and the limitations imposed on us from outside, are tracked in
[`known-issues.md`](known-issues.md).

## Planned

### A display of our own for `rxa`

The first of the three routes below is **implemented**, behind the agent's
**Add a private 2x display** setting (see
[`mac-agent-architecture.md`](mac-agent-architecture.md)). `CGVirtualDisplay`
adds a monitor beside a Mac's own screens rather than replacing one, so the
setting decides only whether that display exists; which display a session shares
is a separate, per-session choice made from either client. What is still open is
whether it should ever be the default — and the two routes not taken, kept
because the private one can be withdrawn by any macOS release.

Both clients present a remote at its own size, scaling by the ratio between the
two densities, so a 1x guest on a Retina host is drawn magnified, which is soft.
What a display of our own would add is the pixels to be sharp with: ask for a
display of the viewport's device-pixel size marked as scale 2, and its logical
size is the window's point size, so the blit is one texel per device pixel *and*
physically right with nothing resampled. Three routes, none of them free:

- **`CGVirtualDisplay` (private CoreGraphics/SkyLight).** What BetterDisplay and
  similar tools use: arbitrary pixel size and a HiDPI backing store, with no
  entitlement to request. Measured on the test VM (macOS 26.5.2, Apple
  Virtualization guest) it does work there. Listing one mode at the *point* size
  with `hiDPI = 1`, `maxPixels` at twice that size, and `sizeInMillimeters` sized
  to put the density near 200 dpi (about `maxPixels / 9`), gives a 1600×1000 pt
  display backed by 3200×2000 real pixels — sharp, on a guest whose own
  paravirtual framebuffer advertises no HiDPI mode at any size. ScreenCaptureKit
  lists and captures it at full resolution, and the display is process-scoped:
  released object, display gone.

  What makes the route treacherous rather than merely unsupported is that two
  readings misreport such a display. `CGDisplayBounds` shows the intended point
  size even when the backing store is 1x, so a wrong configuration looks right
  until something captures native pixels. `SCContentFilter.pointPixelScale`
  reports 1.00 on a genuine 2x display, so capture size has to be set explicitly
  instead of derived from the filter. (A third was listed here and was wrong:
  `CGDisplayCopyDisplayMode` does work on these displays and reports the true
  backing scale — see [`mac-agent-architecture.md`](mac-agent-architecture.md).
  `NSScreen.backingScaleFactor` is the one that takes its place, reporting 2.00
  for a display that is genuinely 1x.) And of five plausible descriptor
  configurations only one produced 2x, because the HiDPI flag does not command
  it: what decides is pixel density, `modePixels / sizeInMillimeters`, which has
  to land in a window of roughly
  145–300 dpi. At a fixed 1000×700 pt mode, 134 and 318 dpi both came up 1x,
  while 149 through 264 dpi came up 2x. Outside that window the display appears
  at twice the requested point size at 1x — a desktop with unreadable UI rather
  than a merely soft one. A major release could keep every symbol and still
  change which configuration works, and this would be load-bearing for the whole
  `rxa` display path.

  Such a display is also a normal display to the rest of macOS: it appears in
  System Settings > Displays with the mode list macOS derives from the
  descriptor — a HiDPI entry and a `(low resolution)` one at each size — so
  whoever is using that Mac changes its resolution there, like any other screen.
  Re-applying settings from the process takes any point size exactly and can flip
  the density, keeping the same `displayID` and settling in 130–580 ms; the agent
  uses that for density only, and the size question is the section below.

  Nothing about the route is VM-specific. It was measured in the guest because
  that is the test machine; these are the same calls BetterDisplay drives on
  physical hardware, so it is available on either. Which machines create one is
  configuration.

- **DriverKit virtual framebuffer.** The supported route. Needs an Apple-granted
  DriverKit entitlement and a system extension the user approves, which is a
  heavier install than the rest of the agent put together, and the entitlement is
  not a given.
- **Host-side, and only for a VM.** An Apple Virtualization guest's scale is
  already the host's to decide:
  `VZMacGraphicsDisplayConfiguration(widthInPixels:heightInPixels:pixelsPerInch:)`
  — a high `pixelsPerInch` gives a HiDPI display — and on macOS 14+
  `automaticallyReconfiguresDisplay` makes the guest's display follow the VM
  window. That is the VM app's configuration (UTM, in the test setup), not
  something an agent inside the guest can drive, so it settles the development
  machine and nothing else.

What is still to be decided: whether a display of our own should ever be the
**default**. Sharing it turns `rxa` from screen sharing into a separate desktop —
nobody's windows get rearranged by a connection, which is the upside, and it
stops being "what is on that Mac's screen", which is the point of `rxa` today,
and leaves a desktop nobody is looking at once the viewer goes away. Since it is
an extra display rather than a replacement, whoever is connecting makes that
trade per session, which is a large part of why it need not be settled here.

It also only helps `rxa`: a Linux or Windows box cannot be handed a display from
here, so for those the scaled presentation is the whole answer.

### Resize the private display to the client's window, on request

Its **density** already follows the client, automatically: both clients report
the backing scale of the screen their window is on
(`GatewayMsg::HostScale`), and a display the agent made matches it, so a session
from a Retina screen gets a 2x desktop and one from a 1x screen gets a 1x desktop
at a quarter of the framebuffer. Its **size** does not follow anything yet, and
that is the piece left.

The shape is RDP's, not VNC's: a button, not continuous following. A reconfigure
relays every window on that desktop, so it must be something the person asks for
rather than something a window drag does forty times. Both clients already have
the control — the floating menu's **Resize to window** — and it is disabled for
`rxa` today.

Measured on the test VM (macOS 26.5.2), the mechanics work: `applySettings:` on
the live display honours an arbitrary point size, keeps the same `displayID`, and
takes 66–397 ms to apply with bounds settled 134–580 ms later. Two limits are
fixed at creation and cannot be changed, and they are what make this a design
rather than a patch:

- **`maxPixels`** is a hard ceiling. Asking past it silently halves the result
  while `applySettings:` still returns YES — so a window larger than the created
  size cannot be matched, only clamped to it.
- **`sizeInMillimeters`** fixes the density, so shrinking the mode walks the
  display down out of the HiDPI window; about 57% of the created width loses 2x,
  and recovering it needs a *new* display, hence a new `displayID` and a new
  identity for macOS to file an arrangement against.

So `virtual_display_initial_size` stops being "the resolution" and becomes what
it already is underneath: the **envelope** — the largest mode that can ever be 2x,
and the range a resize has to work inside. Which is also the answer to "should
the user change it": not as a resolution, only as a ceiling, once.

## Deferred pending measurements

### Retina performance for `rxa`

The current adaptive PNG/JPEG tile path has not been characterized on a Retina
desktop. If it cannot keep up, optimize in this order:

1. downscale through `SCStreamConfiguration`;
2. use coarser tiles;
3. move to VideoToolbox H.264 and browser WebCodecs.

H.264 is last because it creates a second browser decode path and adds stream
state that the current independent tile protocol avoids.

### Application-level liveness for VNC

Socket keepalive bounds host death and network partition
([`architecture.md`](architecture.md)), but it is answered by the peer's kernel,
so for RDP and VNC a server that is hung while still on the network reads as an
idle desktop ([`known-issues.md`](known-issues.md)) — RXA's ping/pong already
closes that half for the agent. RFB has exactly one message a conformant
server must answer regardless of change: a **non-incremental**
`FramebufferUpdateRequest`. A 1×1 one at the origin is therefore a ~10-byte probe,
and `update_request` already builds the shape — it would need the region
parameterised.

Three things to measure before committing to it. There is no correlation nonce, so
liveness could only mean "some update arrived within the deadline". Servers union
pending requests, and while libvncserver and TigerVNC add a non-incremental region
to the modified region (so only the 1×1 comes back), a server that instead applies
the flag to the whole union would retransmit the entire framebuffer every probe —
4 MB at 1280×800 raw. And each probe pushes a 1×1 tile through to the client.

RDP has no equivalent to offer. Its Heartbeat PDU is server-to-client only and
undecoded by `ironrdp-pdu`; Refresh Rect would force a round trip but may not be
sent unless the server advertised `refreshRectSupport`, which IronRDP's
`ConnectionResult` does not expose. That needs an upstream change first.

### Capture-stream linger

Keeping `SCStream` alive briefly after a gateway disconnect could avoid capture
teardown during a network blip. It is worthwhile only if stream restart time is
material relative to the gateway's one-second minimum reconnect backoff.
Implementing it would move capture ownership from the session task to
agent-level state.

### Encoder parallelism

A worker pool can complete tiles out of order. When consecutive frames update
the same region, a late tile from the older frame could overwrite newer pixels.
Keep ordered single-worker encoding unless measurements justify adding explicit
ordering.

### Audio

No engine currently carries audio. Its transport, synchronization, and browser
playback design remain unspecified.


### macOS login-window service

The `SMAppService` LaunchAgent runs only in the signed-in user's Aqua session, so
the agent stops at logout and cannot be reached from the macOS login screen.

The shape of an answer is established: RealVNC's
[Service Mode](https://help.realvnc.com/hc/en-us/articles/360002253238-Understanding-RealVNC-Server-Modes)
and RustDesk's installed-service mode both provide login-window access, by
installing launch components that declare the `LoginWindow` session type
alongside `Aqua` instead of relying on per-user `SMAppService`.

Nothing past that shape is settled, and none of it should be designed here first.
What has to be measured on a real Mac: how the single listener is held across the
login transition and fast user switching, where config and PSK readable by both
the configured user and the UID 0 login-window process should live, and whether
Screen Recording and Accessibility grants reach the signed app in the
`LoginWindow` session at all.

FileVault is the one boundary none of it crosses: no remote-access process runs
before pre-boot disk unlock.


## Not planned

### Multiple sessions

remotex permanently has one active session slot. A new browser takes over and
evicts the previous holder. Concurrent sessions, shared sessions, and a session
broker are outside the product model.
