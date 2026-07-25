# Roadmap

## Planned

### RDP clipboard

The clipboard bridge is implemented for VNC and `rxa` (see
[`architecture.md`](architecture.md)); RDP is the remaining engine, and
`clipboard = true` is rejected for RDP targets until it lands.

It is the largest of the three by some margin: MS-RDPECLIP is a static virtual
channel with a capability exchange and a delayed-rendering handshake (Format
List, then Format Data Request/Response per paste), against VNC's two cut-text
messages. It needs the `cliprdr` feature of IronRDP, a second
`with_static_channel` registration in `src/rdp.rs`, and a way to drive the SVC
processor from `active_loop`.

The browser side needs nothing: the wire protocol, the config flag, and the
panel are already in place and engine-agnostic.

### Unicode clipboard for VNC

VNC clipboard text is latin-1, so anything outside it becomes `?` on the way to
the remote. RFB's answer is the Extended Clipboard pseudo-encoding
(`0xc0a1e5ce`): UTF-8, zlib-compressed, behind a capability handshake and a
lazy Notify/Request/Provide exchange. Worth doing only if the `?` turns out to
matter in practice — `rxa` targets already carry full UTF-8.

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

### Dynamic resize for `rxa`

The agent mirrors a physical display. Resizing it to the browser viewport would
change the Mac's actual display mode, rearrange local windows, and leave the
machine altered after the browser disconnects. This differs from resizing an
RDP virtual desktop or a virtual VNC server.

An isolated, session-sized desktop would require a virtual display. macOS has
no suitable public API; DriverKit or private `CGVirtualDisplay` integration
would also change `rxa` from screen sharing into a separate desktop. Therefore
`resize = true` is rejected for `rxa` targets.

### Multiple sessions

remotex permanently has one active session slot. A new browser takes over and
evicts the previous holder. Concurrent sessions, shared sessions, and a session
broker are outside the product model.
