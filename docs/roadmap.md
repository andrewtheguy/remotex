# Roadmap

## Planned

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
altered after the browser disconnects. On a **virtual** display the guest cannot
take an arbitrary size at all — it switches between the modes the host
advertises, so following the viewport would mean a mode switch on every window
drag, each landing on a neighbouring size nobody asked for, and each risking the
display-stack wedge that only a VM reboot clears.

An isolated, session-sized desktop would need a virtual display of our own.
macOS has no suitable public API; DriverKit or private `CGVirtualDisplay`
integration would also change `rxa` from screen sharing into a separate desktop.

### Multiple sessions

remotex permanently has one active session slot. A new browser takes over and
evicts the previous holder. Concurrent sessions, shared sessions, and a session
broker are outside the product model.
