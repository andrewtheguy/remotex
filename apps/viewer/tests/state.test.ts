// The rules every menu is derived from, and the one input decision this app makes.

import { describe, expect, test } from "bun:test";
import {
  acceptState,
  autoResizeLabel,
  blankState,
  canAudio,
  canAutoResize,
  canClipboard,
  canOverrideMacKeys,
  canResizeNow,
  canResizeToDisplay,
  clipboardShouldFollow,
  displaySummaryLine,
  guestOwnsKeyboard,
  isOnDesktop,
  macKeyOverridesLabel,
  takeOverTitle,
  type ViewerState,
  windowTitle,
} from "../src/main/state.ts";
import type { NativeState } from "../src/shared/contract.ts";

function live(overrides: Partial<NativeState> = {}): ViewerState {
  return {
    screen: { phase: "ready" },
    state: {
      ...blankState(),
      mode: "desktop",
      status: "connected",
      ready: true,
      ...overrides,
    },
  };
}

describe("what the page reported", () => {
  test("a partial report reads as nothing connected, not as a failure", () => {
    // A page mid-navigation posts what it has. The answer to a missing field is
    // "nothing is connected", not menus describing the session before it.
    const state = acceptState({ mode: "desktop" });
    expect(state.mode).toBe("desktop");
    expect(state.status).toBe("connecting");
    expect(state.canClipboard).toBe(false);
    expect(state.branding).toBe("remotex");
  });

  test("a field of the wrong shape takes the fallback", () => {
    const state = acceptState({ ready: "yes", displays: "none", size: 5 });
    expect(state.ready).toBe(false);
    expect(state.displays).toEqual([]);
    expect(state.size).toBeNull();
  });

  test("a size that is not a size is no size", () => {
    // `NaN` and `Infinity` are numbers, and from here they reach a window resize and
    // a menu that reads "Remote NaN × NaN".
    for (const size of [
      { w: Number.NaN, h: 1080, scale: 1 },
      { w: 1920, h: Number.POSITIVE_INFINITY, scale: 1 },
      { w: 1920, h: 1080, scale: 0 },
      { w: -1920, h: 1080, scale: 1 },
    ]) {
      expect(acceptState({ size }).size).toBeNull();
    }
    const good = { w: 1920, h: 1080, scale: 2 };
    expect(acceptState({ size: good }).size).toEqual(good);
  });

  test("a display list is taken whole or not at all", () => {
    const display = {
      id: 1,
      label: "Display 1",
      detail: "1920×1080",
      main: true,
      virtual: false,
    };
    expect(acceptState({ displays: [display] }).displays).toEqual([display]);
    // One malformed entry discredits the list: a menu that silently drops a screen
    // is harder to notice than one that offers none.
    expect(acceptState({ displays: [display, { id: 2 }] }).displays).toEqual(
      [],
    );
    expect(acceptState({ displays: ["Display 1"] }).displays).toEqual([]);
  });

  test("something that is not a report at all is the blank state", () => {
    expect(acceptState(null)).toEqual(blankState());
    expect(acceptState("state")).toEqual(blankState());
  });
});

describe("the keyboard rule", () => {
  test("a live desktop owns every chord", () => {
    expect(guestOwnsKeyboard(live())).toBe(true);
  });

  test("the picker does not, so Command-Q quits", () => {
    // This is what makes giving ⌘Q away safe: move off the desktop and it is an
    // ordinary Quit again.
    expect(guestOwnsKeyboard(live({ mode: "picker" }))).toBe(false);
  });

  test("a desktop with no frame yet does not", () => {
    expect(guestOwnsKeyboard(live({ ready: false }))).toBe(false);
  });

  test("a caret in a text box hands the shortcuts back", () => {
    // Command-V has to reach the clipboard panel's field, and the Edit menu is the
    // only thing that can do that.
    expect(guestOwnsKeyboard(live({ editing: true }))).toBe(false);
  });

  test("the launch screen never owns the keyboard", () => {
    expect(
      guestOwnsKeyboard({
        screen: { phase: "launching" },
        state: live().state,
      }),
    ).toBe(false);
  });
});

describe("what is on the desktop", () => {
  test("a session command needs a connected desktop", () => {
    expect(isOnDesktop(live())).toBe(true);
    expect(isOnDesktop(live({ status: "reconnecting" }))).toBe(false);
    expect(isOnDesktop(live({ mode: "picker" }))).toBe(false);
  });

  test("clipboard and audio follow the target's own permissions", () => {
    expect(canClipboard(live())).toBe(false);
    expect(canClipboard(live({ canClipboard: true }))).toBe(true);
    expect(canAudio(live({ canAudio: true }))).toBe(true);
    // …and nothing is available off the desktop, whatever the target allows.
    expect(canClipboard(live({ mode: "picker", canClipboard: true }))).toBe(
      false,
    );
  });

  test("the clipboard follows the pasteboard on the desktop, connected or not", () => {
    // Deliberately not `isOnDesktop`: a reconnecting session is one whose clipboard
    // is about to matter again, and stopping the poll would drop what was copied
    // meanwhile.
    expect(
      clipboardShouldFollow(
        live({ status: "reconnecting", canClipboard: true }),
      ),
    ).toBe(true);
    expect(
      clipboardShouldFollow(live({ mode: "picker", canClipboard: true })),
    ).toBe(false);
  });
});

describe("resizing", () => {
  test("auto resize needs both permissions", () => {
    expect(canAutoResize(live({ canResize: true }))).toBe(false);
    expect(canAutoResize(live({ canResize: true, canAutoResize: true }))).toBe(
      true,
    );
  });

  test("a target that may resize but not follow the window says so", () => {
    // Greying alone would read as "this session cannot resize", which the item
    // below it disproves.
    expect(autoResizeLabel(live({ canResize: true }))).toBe(
      "Auto Resize (Not Applicable)",
    );
    expect(
      autoResizeLabel(live({ canResize: true, canAutoResize: true })),
    ).toBe("Auto Resize");
  });

  test("one resize now is refused while the window is driving the size", () => {
    expect(canResizeNow(live({ canResize: true }))).toBe(true);
    expect(canResizeNow(live({ canResize: true, autoResize: true }))).toBe(
      false,
    );
  });

  test("fitting the window needs a size, not a permission", () => {
    const size = { w: 1920, h: 1080, scale: 1 };
    expect(canResizeToDisplay(live({ size }))).toBe(true);
    // Nothing is sent for it, so a target that refuses resizing still gets it…
    expect(canResizeToDisplay(live({ size, canResize: false }))).toBe(true);
    // …but not while the size is about to follow the window back.
    expect(
      canResizeToDisplay(live({ size, canResize: true, autoResize: true })),
    ).toBe(false);
    expect(canResizeToDisplay(live())).toBe(false);
  });
});

describe("titles", () => {
  test("the window says which instance it is, and whether sound is playing", () => {
    expect(windowTitle(live({ branding: "work" }))).toBe("work");
    expect(windowTitle(live({ branding: "work", audioEnabled: true }))).toBe(
      "work 🔊",
    );
  });

  test("take over appears only when somebody else holds the session", () => {
    expect(takeOverTitle(live())).toBeNull();
    expect(takeOverTitle(live({ status: "busy" }))).toBe("Take Over Session");
    expect(takeOverTitle(live({ status: "takenOver" }))).toBe(
      "Take Session Back",
    );
  });

  test("a Mac guest has nothing to override, and the label says which", () => {
    expect(macKeyOverridesLabel(live())).toBe(
      "Enable macOS Keyboard Overrides",
    );
    expect(macKeyOverridesLabel(live({ remoteIsMac: true }))).toBe(
      "Enable macOS Keyboard Overrides (Not Applicable)",
    );
    expect(canOverrideMacKeys(live({ remoteIsMac: true }))).toBe(false);
  });

  test("the display readout names the remote in its own terms", () => {
    expect(
      displaySummaryLine(live({ size: { w: 1920, h: 1080, scale: 1 } }), 2),
    ).toBe("Remote 1920 × 1080 px · This Display 2×");
    // A Retina guest: the logical desktop and the pixels it draws are different
    // numbers, and both are worth saying.
    expect(
      displaySummaryLine(live({ size: { w: 3840, h: 2160, scale: 2 } }), 2),
    ).toBe("Remote 1920 × 1080 pt (3840 × 2160 px) · This Display 2×");
  });
});
