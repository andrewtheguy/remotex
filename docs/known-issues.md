# Known issues

What a user can run into and be surprised by, including limitations imposed on
us from outside. This is the one place for them: behaviour that is merely
*designed* belongs in the architecture docs, and behaviour that is *planned*
belongs in [`roadmap.md`](roadmap.md). Each entry says what is seen and points
at wherever the mechanism is already explained rather than repeating it.

Fixed issues are not kept here. The commit that fixed one, and the test that
holds it fixed, are the record.

## An RDP or VNC server that is hung but still on the network looks live

- **Area:** `protocol = "rdp"` and `protocol = "vnc"`. Not `rxa`, which asks the
  agent process itself — see the entry below for what that still misses.
- **Seen:** the desktop stops updating and stays on screen. No error, no return
  to the picker — the client cannot tell it from a desktop nobody is touching.
  Switching target by hand is the only way out.
- **Cause:** for these two, liveness is the kernel's to report, and TCP keepalive
  probes are answered by the peer's *kernel* with no involvement from the server
  process. So a wedged `Xvnc`, a sleeping display, a `SIGSTOP`ped server, or a VM
  that was *suspended* rather than powered off all keep answering. Neither
  protocol offers a way to ask better: RFB has no ping, and IronRDP's Heartbeat
  PDU is server-to-client only while its Refresh Rect PDU may not be sent unless
  the server advertised `refreshRectSupport`, which the connector does not
  expose. A probe that would close the RFB half is in
  [`roadmap.md`](roadmap.md).
- **Mitigation:** what *is* bounded is host death and network partition, at about
  25 seconds (30 on Linux for a socket with traffic outstanding) — see the socket
  policy in [`architecture.md`](architecture.md). A remote that is switched off,
  unplugged, or has its lid closed reports an error and returns to the picker.
- **Guard:** none in CI. Reaching the case needs a peer that stops answering
  without closing, which means root or a container network — by hand:
  `docker network disconnect <net> <container>` (or `podman`) against a
  `tests/vnc-dummy` container mid-session, then expect the picker in ~25s.

## An `rxa` agent can answer while its capture has stopped

- **Area:** `protocol = "rxa"`.
- **Seen:** the desktop stops updating with the session still reported as live.
  Rarer than the RDP/VNC case above, and narrower.
- **Cause:** the ping/pong between gateway and agent is answered by the agent
  *process*, so it catches a Mac whose agent has wedged or gone — which is what
  keepalive alone cannot do. What it does not prove is that pixels are still
  flowing: the agent answers pings from its message loop while capture delivers on
  its own queues, so a capture stream that is alive but producing nothing — a
  wedged WindowServer, a display that has stopped delivering — leaves the link
  provably healthy and the picture frozen. A capture pipeline that *ends* is a
  different case and is reported.
- **Mitigation:** none beyond the above. Switching target re-attaches and starts
  a fresh capture stream.
- **Guard:** none. Unlike the entry above this has not been reproduced
  deliberately — it is the gap the ping/pong leaves by construction, recorded so
  a frozen `rxa` desktop with a healthy link is not mistaken for the RDP/VNC
  case, which has a different cause and a different bound.

## A VM's display stack can wedge until the VM reboots

- **Area:** resolution changes inside an Apple Virtualization (UTM) guest.
- **Seen:** after a number of resolution changes, every further one hangs —
  `CGCompleteDisplayConfiguration` never returns and the calling thread spins at
  around 40% CPU. Nothing but a reboot of the guest clears it.
- **Not remotex's to trigger any more:** nothing here changes a display's mode,
  so this is reached only by changing the resolution on the Mac itself. The
  agent keeps capturing whatever the display last settled on.
- **Guard:** none possible off a real VM.

## The browser can render the virtual display jagged

- **Area:** `protocol = "rxa"` sharing the private display the agent adds, in the
  browser. Not reported on the Mac's own screens, and not reported in the native
  viewer on the same display.
- **Seen:** the image is **jagged** — not soft or blurred, which would be a
  different symptom with a different cause. Sometimes, rather than every session.
- **Cause: unknown, and not yet characterised.** What is ruled out is the wire:
  the gateway relays the agent's PNG/JPEG bytes without decoding them
  ([`architecture.md`](architecture.md)), so both clients receive identical tiles
  and the difference is downstream of them arriving. Nothing beyond that has been
  established — including whether it depends on the display's mode, the browser
  window size, or the touch zoom, none of which has been varied against it.
- **Workaround:** the native viewer, on the same session and the same display.
- **Guard:** none, and none is possible from the current tests: paint quality is
  out of scope for browser automation (see `CLAUDE.md`), so this needs a human
  looking at the picture. Do not close it on a reading of the code — it was
  reported from a real session and no cause has been found.

## macOS can hold a virtual display's identity offline

- **Area:** the agent with **Add a private 2x display** ticked.
- **Seen:** the agent logs that it created a display, but nothing can capture
  it: it is missing from the active display list and from ScreenCaptureKit —
  and so from the display picker in both clients — while `CGDisplayBounds` still
  reports exactly the size that was asked for.
- **Cause:** the WindowServer remembers arrangement state against a display's
  vendor, product and serial — as it does for any monitor — and can decide to keep
  that identity offline. The state survives a reboot and cannot be cleared from
  inside the process. Observed 2026-07-27 against an identity earlier probe builds
  had used, which is the condition most likely to produce it: several descriptors
  claiming one identity.
- **Reported, not worked around:** creation checks `CGDisplayIsOnline` and
  `CGDisplayIsActive` rather than trusting bounds, and says so in the log and to
  the client. The agent deliberately does *not* mint a new identity to escape it
  — that would produce a working display by discarding the arrangement the
  identity stands for, which is the one thing a real monitor never does to you
  (see [`mac-agent-architecture.md`](mac-agent-architecture.md)).
- **Fix:** on the Mac, where display arrangement lives — System Settings >
  Displays, resetting the arrangement. The same place you would go for a panel
  that came back dark.
- **Guard:** none possible in CI; it needs a real WindowServer in a state that
  cannot be created on demand.

## macOS Screen Sharing ignores `SetDesktopSize`

- **Area:** a Mac target reached over VNC (`protocol = "vnc"`) with
  `resize = true`.
- **Seen:** the desktop never resizes, with no error. The gateway advertises
  DesktopSize/ExtendedDesktopSize and sends the request after the server
  confirms support; Apple's server accepts the negotiation and does nothing with
  the request. Observed 2026-07-23 against the test VM.
- **Workaround:** change the resolution on the Mac itself. Nothing a client
  sends can do it, over VNC or otherwise.

## Changing the agent's signing identity silently breaks its permissions

- **Area:** installing a `remotex-agent.app` built with a different code
  signing identity than the one already granted — including an ad-hoc GitHub
  release over a locally signed build, or the reverse.
- **Seen:** capture and input stop working with no prompt and no obvious cause.
  System Settings still lists `remotex-agent` under Screen Recording and
  Accessibility with its box ticked, because the entry is matched by path, while
  the system refuses the app behind it.
- **Fix:** in *both* panes, remove `remotex-agent` with "−", add it back with
  "+", and reopen the agent.
- **Avoid:** keep one Developer ID identity for every build of the agent; see
  [`packaging/macos/README.md`](../packaging/macos/README.md).

## Gateway can remain alive after its launcher is closed

- **Area:** launcher integration / local server lifecycle.
- **Seen:** intermittent. A gateway launched with `cargo run -- serve` remained
  listening after the launching automation session was interrupted.
- **What has been ruled out:** direct SIGINT and SIGTERM both exit and release
  the port; SIGHUP and closing the controlling PTY both terminate the process
  and release the port; a `cargo run` probe left no separate Cargo launcher
  process that could orphan the gateway. No shutdown failure has been reproduced
  when an actual shutdown event reaches the gateway.
- **Next investigation:** capture the process tree and signal behaviour of the
  launcher's cancellation path. Do not change gateway shutdown behaviour until
  that path is reproduced.
- **Required guard:** a subprocess lifecycle test for any confirmed failing exit
  path, or a launcher test if cancellation is confirmed to omit teardown.
