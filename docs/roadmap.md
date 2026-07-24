# Roadmap

Phases 1–10 are done and their planning docs were removed;
[`architecture.md`](architecture.md) describes the system as built. What
remains, in planned order:

- **Phase 11 — the rename:** when the project is ready,
  rename the GitHub repo to **remotex** and the binary to
  **remotex**. Not done now; documented here so it
  isn't forgotten.
- **Phase 12 — multi-target support:** target picker over the `[[targets]]`
  config (still one active session at a time).
- **Phase 13 — clipboard bridge:** replace the **Clipboard** placeholder with
  a real text-clipboard sync. The backend holds the clipboard contents
  server-side (a single stored buffer, matching the one-active-session model),
  updated from the remote (`ServerCutText` for VNC, the RDP clipboard channel —
  today the VNC engine drops it) and from the browser, and pushes changes the
  other way so copy/paste crosses the browser ↔ remote boundary in both
  directions.
