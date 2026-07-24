# Roadmap

Phases 1–6 are done and their planning docs were removed;
[`architecture.md`](architecture.md) describes the system as built. What
remains, in planned order:

- **Phase 7 — soft keyboard + the floating UI:** port remotex's soft
  keyboard mapping/panel and the fancy floating chrome (the draggable 三
  button that opens the toolbar).
- **Phase 8 — multi-target support:** target picker over the `[[targets]]`
  config (still one active session at a time).
- **Phase 9 — the rename:** when the project is ready to replace the old
  one, rename the GitHub repo to **remotex-v2** and the binary to
  **remotex**, replacing the original. Not done now; documented here so it
  isn't forgotten.

Also outstanding (not a phase): verify the VNC engine against real targets —
including macOS Screen Sharing, the case that motivated the raw-baseline,
no-workarounds approach.
