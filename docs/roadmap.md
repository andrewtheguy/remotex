# Roadmap

Phases 1–9 are done and their planning docs were removed;
[`architecture.md`](architecture.md) describes the system as built. What
remains, in planned order:

- **Phase 10 — soft keyboard:** port remotex's soft keyboard mapping/panel.
  The floating toolbar's **Soft keyboard** button is a stub that alerts
  "not implemented yet" until this lands; the toolbar's functional controls
  (special keys, modifier taps, gesture help) and the **Clipboard** placeholder
  (the VNC engine still drops `ServerCutText`) already ship in phase 9.
- **Phase 11 — the rename:** when the project is ready to replace the old
  one, rename the GitHub repo to **remotex-v2** and the binary to
  **remotex**, replacing the original. Not done now; documented here so it
  isn't forgotten.
- **Phase 12 — multi-target support:** target picker over the `[[targets]]`
  config (still one active session at a time).

Also outstanding (not a phase): verify the VNC engine against real targets —
including macOS Screen Sharing, the case that motivated the raw-baseline,
no-workarounds approach.
