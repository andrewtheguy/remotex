# Known issues

Active defects found during manual end-to-end testing live here until they are
fixed and covered by a regression test. Keep resolved entries in the bottom
section so the original reproduction and its guard are not lost.

## Open

### RX-003 — Gateway can remain alive after its launcher is closed

- **Found:** 2026-07-25
- **Area:** launcher integration / local server lifecycle
- **Reproduction:** Intermittent. A gateway launched with `cargo run -- serve`
  remained listening after the launching Codex terminal session was
  interrupted.
- **QA matrix:** Direct SIGINT and SIGTERM both exit successfully and release
  the listening port. SIGHUP and closing the controlling PTY both terminate
  the process and release the port. A local `cargo run` probe did not leave a
  separate Cargo launcher process that could orphan the gateway.
- **Known boundary:** The only lingering case observed so far was an
  interrupted Codex automation session that left the gateway alive and
  delivered no signal or controlling-terminal close. No gateway shutdown
  failure has been reproduced when an actual shutdown event reaches it.
- **Next investigation:** Capture the exact process-tree and signal behavior
  of the launcher cancellation path. Keep this issue open against the launcher
  integration; do not change gateway shutdown behavior until that path is
  reproduced.
- **Required guard:** A subprocess lifecycle test for any confirmed failing
  application exit path, or a launcher test if cancellation is confirmed to
  omit teardown.

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
