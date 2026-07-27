# Roadmap

Defects, and the limitations imposed on us from outside, are tracked in
[`known-issues.md`](known-issues.md).

## Planned

### A display of our own for `rxa`

The first of the three routes below is **implemented**, behind the agent's
`virtual_display` setting (see
[`mac-agent-architecture.md`](mac-agent-architecture.md)). What is still open is
whether it should ever be the default, which is the question at the end of this
section — and the two routes not taken, kept because the private one can be
withdrawn by any macOS release.

Both clients now present a remote at its own size, scaling by the ratio between
the two densities, so a Retina host no longer draws a 1x guest at half size — it
draws it magnified, which is soft. What a display of our own would add is the
pixels to be sharp with: ask for a display of the viewport's device-pixel size
marked as scale 2, and its logical size is the window's point size, so the blit is
one texel per device pixel *and* physically right with nothing resampled. Three
routes, none of them free:

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

  What makes the route treacherous rather than merely unsupported is that three
  readings misreport such a display. `CGDisplayBounds` shows the intended point
  size even when the backing store is 1x, so a wrong configuration looks right
  until something captures native pixels. `SCContentFilter.pointPixelScale`
  reports 1.00 on a genuine 2x display, so capture size has to be set explicitly
  instead of derived from the filter. `CGDisplayCopyDisplayMode` returns NULL and
  `CGDisplayCopyAllDisplayModes` returns nothing, so geometry cannot be read the
  usual way. And of five plausible descriptor configurations only one produced
  2x, because the HiDPI flag does not command it: what decides is pixel density,
  `modePixels / sizeInMillimeters`, which has to land in a window of roughly
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
  (Re-applying settings from the process would also take any point size exactly,
  keeping the same `displayID` and settling in 130–580 ms, but there is no reason
  to: resolution is the Mac's, and every reconfiguration rearranges the windows
  on it.)

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

What has to be decided before any of them: a display of our own turns `rxa` from
screen sharing into a **separate desktop**. Nobody's windows get rearranged by a
connection any more, which is the upside — and it stops being "what is on that
Mac's screen", which is the point of `rxa` today, and leaves a desktop nobody is
looking at once the viewer goes away. It also only helps `rxa`: a Linux or Windows
box cannot be handed a display from here, so for those the scaled presentation is
the whole answer.

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
so a remote that is hung while still on the network reads as an idle desktop
([`known-issues.md`](known-issues.md)). RFB has exactly one message a conformant
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

The current `SMAppService` LaunchAgent runs only in the signed-in user's Aqua
session. Login-window access is nevertheless an established macOS deployment
mode: RealVNC provides it in
[Service Mode](https://help.realvnc.com/hc/en-us/articles/360002253238-Understanding-RealVNC-Server-Modes),
and RustDesk ships it in its installed-service mode.

RustDesk's minimum design is comparatively small. A root LaunchDaemon runs its
system service at boot, while one root-installed
[LaunchAgent](https://github.com/rustdesk/rustdesk/blob/master/src/platform/privileges_scripts/agent.plist)
declares both `LoginWindow` and `Aqua` session types and runs the same
`--server` executable in each. Its
[install script](https://github.com/rustdesk/rustdesk/blob/master/src/platform/privileges_scripts/install.scpt)
writes both launchd plists and copies the user's config into `/var/root` for the
login-window process. launchd handles the normal session transition; RustDesk
reconnects after login rather than transferring a live connection between
processes. The much larger multi-user and update code in RustDesk is hardening,
not a prerequisite for basic login-window capture and input.

For remotex, launchd may briefly overlap the `LoginWindow` and `Aqua` agents
during login, and fast user switching can leave more than one Aqua agent alive.
The direct listener still does not require a broker: treat ownership of
`/dev/console` as a lease. Only UID 0 at the login window, or the user who
currently owns the console, may bind port 52381. Inactive agents wait without a
listener; the newly active agent retries until the previous owner releases the
port. This preserves the one-active-session model without `SO_REUSEPORT` or
simultaneous sharing.

Implementation requires:

1. a one-time administrator action that secures the app bundle and installs a
   LaunchAgent for both `LoginWindow` and `Aqua`, instead of relying only on
   per-user `SMAppService`;
2. config and PSK storage readable by both the configured user and the UID 0
   login-window process;
3. active-console listener acquisition, release, and retry around the existing
   agent server;
4. validation of Screen Recording and Accessibility for the signed app in the
   `LoginWindow` session, plus boot, login, logout, lock, and fast-user-switch
   lifecycle tests.

This does not inherently require a package installer, root broker, new
capture/input implementation, or seamless process handoff. Stable Developer ID
signing would make TCC identity reliable across upgrades. FileVault remains the
unavoidable boundary: no remote-access process can run before pre-boot disk
unlock.


## Not planned

### Resolution control from a client

`resize = true` drives the desktop size from the client's window, and only two
protocols get it: VNC, which follows continuously because `SetDesktopSize` is
cheap, and RDP, on request, because its Deactivation-Reactivation is not.
Those are the two whose protocols hand the desktop size to the client.

Nothing else will get it, and no client will ever offer a *menu* of resolutions.
A remote's resolution belongs to the machine running it. On a physical display
there is nothing to resize but the panel in front of a person: it would rearrange
their windows and leave the machine altered after the client disconnects. On a
Mac — including one sharing a display the agent created for itself, which shows
up in System Settings like any other screen — the mode is chosen there, and the
agent reports whatever it lands on.

A VM guest makes the point twice over. Its default screen is not even the
guest's: Apple Virtualization sizes it from the host, and UTM leaves
`automaticallyReconfiguresDisplay` on, so it follows the VM window and lands on
sizes nothing inside the guest asked for. A remotex client competing for that
same decision would be a third party to it.

A client whose window does not match the remote presents the remote at its own
size and scales it to fit, which is what makes following unnecessary rather than
merely unwise.

### Multiple sessions

remotex permanently has one active session slot. A new browser takes over and
evicts the previous holder. Concurrent sessions, shared sessions, and a session
broker are outside the product model.
