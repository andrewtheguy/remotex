# Roadmap

## Planned

### Clipboard bridge

Replace the frontend's Clipboard placeholder with bidirectional text clipboard
sync for RDP, VNC, and `rxa`.

The backend owns one clipboard buffer, matching the one-active-session model.
Remote updates (`ServerCutText` for VNC and the RDP clipboard channel) update
that buffer and are pushed to the browser; browser updates travel in the other
direction. `rxa` needs corresponding protocol messages.

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

### macOS login-window service

The agent is a LaunchAgent in the logged-in user's GUI session, where its
Screen Recording and Accessibility grants apply. Root privilege does not
bypass TCC, and the login window has no user whose grants the agent can use.

This is not a solved baseline among mature remote-desktop products. RustDesk
uses a privileged service plus an agent in the `LoginWindow` session; its
[boot/login-window setup](https://github.com/rustdesk/rustdesk/discussions/7762)
requires extra launchd and MDM work beyond a normal install. RealVNC advertises
login-screen access in
[Service Mode](https://help.realvnc.com/hc/en-us/articles/360002253238-Understanding-RealVNC-Server-Modes),
but still has reports of a
[black login screen after logout](https://help.realvnc.com/hc/en-us/community/posts/17967170655133-Black-screen-only-on-MacOS-login)
and has acknowledged macOS Sonoma login-screen input failures in its
[release notes](https://help.realvnc.com/hc/en-us/articles/360002253138-Release-Notes-v7-7-13-1-and-earlier).
Their experience shows that adding a service process does not remove the
capture, input, and session-transition problems.

Supporting the login window would require a different deployment:

1. a root-installed LaunchAgent supporting both `Aqua` and `LoginWindow`
   sessions, since `SMAppService` cannot express this;
2. system-level config and PSK storage;
3. handoff between login-window and user agents without both owning port 52381;
4. a supported way to authorize capture and input at the login window.

That would replace the current drag-to-Applications install with a privileged,
signed, notarized installer and still leave the TCC question unresolved.
FileVault also prevents any agent from running before pre-boot disk unlock.

The practical alternatives are automatic login after FileVault unlock, or a
separate VNC target using macOS Screen Sharing when access specifically to the
login window is required.

### Multiple sessions

remotex permanently has one active session slot. A new browser takes over and
evicts the previous holder. Concurrent sessions, shared sessions, and a session
broker are outside the product model.
