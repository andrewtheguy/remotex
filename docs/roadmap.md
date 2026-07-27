# Roadmap

Active defects and their required regression guards are tracked in
[`known-issues.md`](known-issues.md).

## Planned

### A display of our own for `rxa`

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

  Such a display also resizes on demand, which the host-provided one in a guest
  cannot. Re-applying settings to a live display takes any point size exactly —
  1234×789 as readily as 1280×800 — keeps the same `displayID`, settles in
  130–580 ms, and keeps its 2x backing across the change (confirmed by native
  capture, not inferred from bounds). Two creation-time fields set the limits.
  `sizeInMillimeters` cannot be changed afterwards, and HiDPI holds only while
  density stays in that window, so shrinking past roughly 57% of the original
  width silently gives a 1x display at twice the requested point size; growing
  back recovers it, but a different density window needs a new display, and so a
  new `displayID`. `maxPixels` is a hard ceiling — beyond it the result is
  silently halved. Every resize is a real reconfiguration, and it rearranges
  windows.

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

### Viewport-following resize for `rxa`

`resize = true` on an `rxa` target offers the browser the resolutions the Mac's
display advertises, and only when that display is a virtual one in a VM (see
`docs/mac-agent-architecture.md`). What it will never do is follow the browser
viewport the way RDP and VNC do.

Two reasons. On a **physical** display there is nothing to resize but the panel
in front of a person: it would rearrange their windows and leave the machine
altered after the browser disconnects. On the **host-provided virtual** display
of a VM guest the guest cannot take an arbitrary size at all — it switches
between the modes the host advertises, so following the viewport would mean a
mode switch on every window drag, each landing on a neighbouring size nobody
asked for, and each risking the display-stack wedge that only a VM reboot
clears. That is a property of that paravirtual framebuffer, not of virtual
displays in general.

A viewport that does not match the Mac's display is presented at the remote's own
size and scaled to fit the window, which is what makes following it unnecessary
rather than merely unwise.

What is *not* settled here is a display of our own. An isolated, session-sized
desktop needs one, and the routes to it are real enough to be written down — see
"A display of our own for `rxa`" under Planned. The arbitrary-size objection does
not carry over to it: a `CGVirtualDisplay` does take any size on demand, keeping
its scale and its `displayID`, so a viewport-following display of our own is a
question of whether the rearranging is worth it — every resize still moves
windows, and sizes far enough from the one it was created for drop to 1x — not
of whether the sizes can be had. Either way that would retire this entry for one
protocol rather than answer it: what stays not planned is following the viewport
on the Mac's *existing* display, for the two reasons above.

### Multiple sessions

remotex permanently has one active session slot. A new browser takes over and
evicts the previous holder. Concurrent sessions, shared sessions, and a session
broker are outside the product model.
