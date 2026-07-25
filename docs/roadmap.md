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

- **Dynamic resize for `rxa`.** The agent captures the Mac's own resolution and
  ignores viewport reports; `resize = true` on an `rxa` target is a config error
  rather than a silent no-op.

  Windows RDP can resize to the browser's viewport because an RDP connection
  gets its **own session** with its own virtual desktop — resizing it disturbs
  nobody, because nobody else is looking at it. A Mac has one console session
  and remotex mirrors the physical screen, so the only lever available is the
  **real display mode**. Pulling it would resize the actual monitor, re-lay out
  every window on it, and leave the Mac that way after the browser closed. That
  is a remote client reaching out and rearranging someone's desk, which is a
  different and much more intrusive thing than VNC's `SetDesktopSize` on a
  virtual server.

  The only route to RDP-like behaviour is a **virtual display** — a second,
  session-scoped desktop sized to the browser — and macOS has no public API for
  one. It would mean either a DriverKit display extension or a private
  `CGVirtualDisplay`, and it would change what the product *is*: you would be
  looking at a separate desktop, not at the Mac's screen. So this is not
  planned unless macOS grows a supported way to present a display that the
  person at the keyboard does not share.

- **Service mode for `rxa` — reaching a Mac nobody is logged into.** The agent
  is a LaunchAgent living in the user's GUI session, because that is the only
  place its two TCC grants exist. Closing the gap is not a feature so much as a
  second deployment shape, and it costs the current packaging story — drag the
  app in, open it once, uninstall by dragging to the Trash — because the plist
  would have to be installed as root.

  **Nobody else has really solved this on current macOS either**, which is the
  main reason not to chase it. RustDesk ships exactly the architecture sketched
  below — a `com.carriez.RustDesk_service` daemon plus an agent loaded for both
  `Aqua` and `LoginWindow` — and still carries open reports of a black screen at
  the login window, no control while a Mac sits there, and keystrokes that never
  reach the lock-screen password field. RealVNC officially supports login-screen
  connections and its own forums carry the same black-screen-after-logout
  reports on Sonoma and Sequoia. Adopting somebody else's agent to get this
  would be adopting an unfinished feature.

  Two states get conflated, and only one is hard. **Logged in with the screen
  locked** is a session the agent is already running in, so it may already work;
  untested here, and RustDesk's lock-screen input bug suggests testing it rather
  than assuming. **Logged out at the login window** needs all of:

  1. A second launchd job with `LimitLoadToSessionType` set to both `Aqua` and
     `LoginWindow`. `SMAppService` cannot express it, so it needs a signed and
     notarized installer package writing into `/Library/LaunchAgents`. It must
     be a LaunchAgent — a LaunchDaemon runs in the global session with no GUI
     context, which is exactly why screen capture there returns nothing.
  2. That job runs with no user, so the config and the pre-shared key move out
     of `~/Library/Application Support` to a system path.
  3. Session hand-off. At login the login-window job unloads and the Aqua one
     loads, and back again at logout; two processes want port 52381, so either
     the listener migrates or a privileged daemon owns the socket and brokers to
     whichever agent currently has a window server. The engine's silent
     reconnect would cover the gap, which is a real advantage of the design.
  4. **TCC.** Both grants are per-user and there is no user at the login window,
     so the SIP-protected system database governs and cannot be edited by hand.
     Whether Screen Recording can be *allowed* through an MDM PPPC payload, as
     opposed to only denied, is the open question — and MDM enrolment is a heavy
     ask for a personal Mac regardless.

  ScreenCaptureKit itself is **not** the blocker: Apple DTS confirms it works in
  the LoginWindow session on macOS 14.4+ (it needed a bug fix), given the
  LaunchAgent above. That was previously listed here as unverified.

  And a floor none of it gets under: **with FileVault on, nothing runs until
  someone unlocks the disk at pre-boot.** No volume, no agent, no login window.
  "Reachable without a login" already has a hard limit on any FileVault Mac.

  What covers the realistic case for nothing: **automatic login.** With
  FileVault the pre-boot unlock already authenticates, so the GUI session — and
  the agent with it — comes up straight after, and the Mac is reachable from
  boot. What stays uncovered is only a deliberate logout, which on a
  single-user machine is thin justification for a privileged system service.
  And for that rare case remotex still has a VNC engine: a second target
  pointing at Apple's own Screen Sharing, which does serve the login window,
  costs no new code. The re-login complaint that motivated `rxa` does not apply
  there, because logging in is the whole point of that connection.
