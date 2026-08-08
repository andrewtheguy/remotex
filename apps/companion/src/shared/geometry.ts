// The viewer's window arithmetic, re-exported rather than copied.
//
// `apps/viewer/src/main/geometry.ts` is exactly this problem already solved: pure,
// importing nothing, covered by `apps/viewer/tests/geometry.test.ts`, and already
// carrying the rule that matters most — the window is fitted to the framebuffer, and
// the framebuffer is never scaled to the window. A second copy here would be a second
// place for "no fit-to-window, no zoom-to-fit" to stop being true.
//
// That it imports nothing is load-bearing, not incidental: this tree type-checks in CI
// with no `apps/viewer/node_modules` installed either.

export type { Rect, Size } from "../../../viewer/src/main/geometry.ts";
export {
  documentSize,
  windowFrameFitting,
} from "../../../viewer/src/main/geometry.ts";
