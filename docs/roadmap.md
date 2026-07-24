# Roadmap

Phases 1–7 are done and their planning docs were removed;
[`architecture.md`](architecture.md) describes the system as built. What
remains, in planned order:

- **Phase 8 — mobile gesture support:** port remotex's touch controls onto
  the input overlay — one-finger tap: left-click at the cursor; hard-press:
  hold left-click; second-finger directional drag: move the cursor while
  hold-drag is active; two-finger tap: right-click; two-finger pinch: zoom;
  two-finger drag: pan when not in hold-drag mode; three-finger swipe:
  scroll (vertical and horizontal).
- **Phase 9 — the rename:** when the project is ready to replace the old
  one, rename the GitHub repo to **remotex-v2** and the binary to
  **remotex**, replacing the original. Not done now; documented here so it
  isn't forgotten.
- **Phase 10 — soft keyboard + the floating UI:** port remotex's soft
  keyboard mapping/panel and the fancy floating chrome (the draggable 三
  button that opens the toolbar). The toolbar should absorb the interim
  phase-7 logout affordances (the reserved Ctrl+Alt+Shift+L chord and the
  Disconnect button below the canvas) as a proper button.
- **Phase 11 — multi-target support:** target picker over the `[[targets]]`
  config (still one active session at a time).

Also outstanding (not a phase): verify the VNC engine against real targets —
including macOS Screen Sharing, the case that motivated the raw-baseline,
no-workarounds approach.
