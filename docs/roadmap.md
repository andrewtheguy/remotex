# Roadmap

- **Phase 13 — clipboard bridge:** replace the **Clipboard** placeholder with
  a real text-clipboard sync. The backend holds the clipboard contents
  server-side (a single stored buffer, matching the one-active-session model),
  updated from the remote (`ServerCutText` for VNC, the RDP clipboard channel —
  today the VNC engine drops it) and from the browser, and pushes changes the
  other way so copy/paste crosses the browser ↔ remote boundary in both
  directions. The macOS agent has no clipboard support either; `rxa` would gain
  a message pair alongside the other two engines.

## Landed

- **macOS agent (`rxa`).** Macs are reached over a purpose-built protocol
  instead of Apple's Screen Sharing, so a reconnect never asks for a login.
  How it works is [`architecture.md`](architecture.md#rxa-srcrxars); why it is
  shaped that way is [`mac-agent-plan.md`](mac-agent-plan.md).

## Deferred, with reasons

- **Dynamic resize for `rxa`.** The agent captures the Mac's own resolution and
  ignores viewport reports; `resize = true` on an `rxa` target is a config
  error rather than a silent no-op. Driving a Mac's display mode from the
  browser means changing the actual display mode, which is a different and much
  more intrusive thing than VNC's `SetDesktopSize`.
- **Hardware H.264 for `rxa`.** Per-tile PNG/JPEG has only ever been measured
  against a 1280×800 1× display, so how it behaves on a Retina desktop is an
  open question. If it does not keep up, the ladder is: downscale in
  `SCStreamConfiguration` → coarser tiles → VideoToolbox with WebCodecs in the
  browser. The last rung is deliberately last: it would add an in-browser
  decoder path, which [`architecture.md`](architecture.md#design-tenets) lists
  as a tenet to avoid.
- **A post-disconnect capture-stream linger for `rxa`.** Keeping `SCStream`
  running for a few seconds after a gateway drops would save a
  teardown/restart cycle across a blip. It only pays for outages shorter than
  the gateway's own 1 s minimum reconnect backoff, and restarting the stream
  costs about as much, so it stays a measured optimisation rather than a guess.
  Doing it means hoisting the stream out of the session task into agent-level
  state.
- **A multi-threaded encoder pool for `rxa`.** A pool lets two frames' tiles
  finish out of order, and the same region is commonly dirty in consecutive
  frames, so an older tile can land on top of a newer one and leave stale pixels
  until something else redraws them. Ordering is worth more than the parallelism
  until measurement says otherwise; the fallback ladder above starts with
  downscaling instead.
- **Audio.** No engine carries it.

## Not planned

- **Service mode for `rxa` — reaching a Mac nobody is logged into**, the way
  RealVNC and RustDesk can. The agent is a LaunchAgent living in the user's GUI
  session, because that is the only place its two TCC grants exist. Closing the
  gap is not a feature so much as a second deployment shape, and it costs the
  current packaging story — drag the app in, open it once, uninstall by dragging
  to the Trash — because the plist would have to be installed as root.

  Worth separating two states that get conflated, because only one is hard.
  **Logged in with the screen locked** is a session the agent is already running
  in; whether ScreenCaptureKit still yields pixels over the lock screen is
  untested, and if it does, that case already works and the remote can type the
  password to unlock. **Logged out at the login window** is the RealVNC-style
  capability, and it needs all of the following, in increasing order of
  difficulty:

  1. A second launchd job with `LimitLoadToSessionType = LoginWindow`, the key
     that gets a process into the loginwindow context. `SMAppService` cannot
     express it, so it needs a signed and notarized installer package writing
     into `/Library/LaunchAgents`.
  2. That job runs as root with no user, so the config and the pre-shared key
     move out of `~/Library/Application Support` to a system path.
  3. Session hand-off. At login the login-window job unloads and the Aqua one
     loads, and back again at logout; two processes want port 52381, so either
     the listener migrates or a privileged daemon owns the socket and brokers to
     whichever agent currently has a window server. The engine's silent
     reconnect would cover the gap, which is a real advantage of the design.
  4. **TCC, which is the wall.** Both grants are per-user and there is no user
     at the login window, so the SIP-protected system database governs and
     cannot be edited by hand. The supported route is an MDM Privacy Preferences
     Policy Control payload — i.e. enrolling the Mac in an MDM.

  **Two things to settle before any of that would be worth starting**, both
  unverified: whether Screen Recording can be *allowed* through a PPPC payload
  at all as opposed to only denied, and whether ScreenCaptureKit even functions
  in the loginwindow session (Apple's own `screensharingd` predates it and uses
  different plumbing). If the first answer is no, the other four items are
  wasted work.

  And a floor none of it gets under: **with FileVault on, nothing runs until
  someone unlocks the disk at pre-boot.** No volume, no agent, no login window.
  "Reachable without a login" already has a hard limit on any FileVault Mac.

  What covers the realistic case for nothing: **automatic login.** With
  FileVault the pre-boot unlock already authenticates, so the GUI session — and
  the agent with it — comes up straight after, and the Mac is reachable from
  boot. What stays uncovered is only a deliberate logout, which on a
  single-user machine is thin justification for a privileged system service.
  That is why this is not planned rather than merely deferred.
