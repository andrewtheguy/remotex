# Known issues

Active defects found during manual end-to-end testing live here until they are
fixed and covered by a regression test. Keep resolved entries in the bottom
section so the original reproduction and its guard are not lost.

## Open

### RX-003 — Gateway can remain alive after its launcher is closed

- **Found:** 2026-07-25
- **Area:** local server lifecycle
- **Reproduction:** Intermittent. A gateway launched with `cargo run -- serve`
  remained listening after the launching Codex terminal session was
  interrupted.
- **Known boundary:** Sending the gateway process SIGINT directly exits
  immediately and releases its port. In the observed lingering case the
  launcher never delivered a shutdown signal, so this may be terminal/launcher
  teardown rather than the gateway ignoring one.
- **Required investigation:** Reproduce separately for Ctrl-C, terminal-window
  close, SIGHUP, SIGTERM, and interrupted automation. Record which signal or
  EOF the gateway receives before changing shutdown behavior.
- **Required guard:** A subprocess lifecycle test for the confirmed failing
  exit path.

## Resolved

### RX-001 — Target picker renders `undefined` for the removed OS field

- **Found and fixed:** 2026-07-25
- **Area:** frontend target picker
- **Cause:** `frontend/src/TargetPicker.tsx` still required and rendered `t.os`,
  but the target API no longer returns an `os` field. Remote OS discovery now
  happens only after connecting.
- **Resolution:** Removed the stale field and rendered only protocol, host, and
  port.
- **QA guard:** Compared the live `/api/targets` response with the rebuilt
  viewer and verified all five picker rows render without `undefined`.

### RX-002 — Viewer username entry can capitalize the first character

- **Found and fixed:** 2026-07-25
- **Area:** macOS viewer login
- **Cause:** Safari/WKWebView text correction remained enabled even though the
  input used `autoCapitalize="off"`.
- **Resolution:** Use `autoCapitalize="none"`, `autoCorrect="off"`, and disable
  spellcheck on the username input.
- **QA guard:** Ordinary key-event typing in the rebuilt viewer leaves `admin`
  lowercase and no longer shows the `Admin` correction suggestion.

### RX-004 — Keyboard override menu looks active for a Mac guest

- **Found and fixed:** 2026-07-25
- **Area:** macOS viewer Remote menu
- **Resolution:** For a Mac guest, the item is unchecked, disabled, and labeled
  `macOS Keyboard Overrides (Not Applicable)`. The stored preference is kept
  for the next Windows or Linux guest.
- **Regression guard:**
  `keyboardOverridesAppearInactiveForAMacWithoutChangingThePreference` in
  `apps/remotex-viewer/Tests/AppModelTests.swift`.
