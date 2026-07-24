# Roadmap

Phases 1–10 are done and their planning docs were removed;
[`architecture.md`](architecture.md) describes the system as built. What
remains, in planned order:

- **Phase 10 — soft keyboard (done):** the floating toolbar's **Soft
  keyboard** button opens an on-screen keyboard panel — a compact docked
  layout with a symbol/nav screen toggle and sticky-modifier badges on narrow
  viewports, a draggable floating PC-keyboard grid at ≥800px. Ported from
  remotex, but re-expressed in DOM `code` strings rather than remotex's raw
  X11 keysyms: because we control the backend, every key routes through the
  existing `keymap.rs` pipeline (DOM code → scancode/keysym for both engines,
  with the remote applying Shift from held modifier state), so no second
  keysym-only input path or protocol change was needed. The **Clipboard**
  placeholder (the VNC engine still drops `ServerCutText`) remains from
  phase 9.
- **Phase 11 — the rename:** when the project is ready to replace the old
  one, rename the GitHub repo to **remotex-v2** and the binary to
  **remotex**, replacing the original. Not done now; documented here so it
  isn't forgotten.
- **Phase 12 — multi-target support:** target picker over the `[[targets]]`
  config (still one active session at a time).

Also outstanding (not a phase): verify the VNC engine against real targets —
including macOS Screen Sharing, the case that motivated the raw-baseline,
no-workarounds approach.
