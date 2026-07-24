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
  See [`architecture.md`](architecture.md#rxa-srcrxars); the implementation
  plan it grew from is [`mac-agent-plan.md`](mac-agent-plan.md).

## Deferred, with reasons

- **Dynamic resize for `rxa`.** The agent captures the Mac's own resolution and
  ignores viewport reports; `resize = true` on an `rxa` target is a config
  error rather than a silent no-op. Driving a Mac's display mode from the
  browser means changing the actual display mode, which is a different and much
  more intrusive thing than VNC's `SetDesktopSize`.
- **Hardware H.264 for `rxa`.** If per-tile PNG/JPEG ever fails to keep up with
  a Retina desktop, the ladder is: downscale in `SCStreamConfiguration` →
  coarser tiles → VideoToolbox with WebCodecs in the browser. The last rung is
  deliberately last: it would add an in-browser decoder path, which
  [`architecture.md`](architecture.md#design-tenets) lists as a tenet to avoid.
- **Login-window access for `rxa`.** The agent needs a GUI session for both of
  its permissions, so it cannot run at the login window. Reaching a logged-out
  Mac is a separate design problem, not a missing feature.
- **Audio.** No engine carries it.
