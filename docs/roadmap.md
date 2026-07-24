# Roadmap

Phases 1–4 are done and their planning docs were removed;
[`architecture.md`](architecture.md) describes the system as built. What
remains, in planned order:

- **Phase 5 — connection-flow UX (rescoped, much reduced):** reconnect after
  a dropped/ended session (today that is a manual page reload) and clearer
  error surfacing. The original "port remotex's frontend shell" is mostly
  superseded — the canvas, renderer, and input handling were built natively
  in this repo, and with decode server-side there is one renderer (the RFB
  decoder, `zrleDecoder`, and the rest of remotex's browser-side engine never
  come along). The soft keyboard and the floating UI stay in phase 7;
  clipboard is not planned (a different approach may be considered instead of
  porting remotex's clipboard sync).
- **Phase 6 — session management:** detach/reattach and remotex-style
  takeover of the single session slot (force-claim evicts the previous
  browser) — backed by the server-owned framebuffer. As always, takeover of
  *the* one session — never concurrent sessions, session sharing, or a
  broker (permanently out of scope; see the tenet in
  [`architecture.md`](architecture.md)).
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
