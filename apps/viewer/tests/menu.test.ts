// The menu bar as a function of one state object — every title, tick and greyed
// item, and the rule that keeps ⌘ chords out of it.

import { describe, expect, test } from "bun:test";
import {
  buildMenuSpec,
  everyItem,
  type MenuContext,
  type MenuItemSpec,
} from "../src/main/menu.ts";
import { blankState, type ViewerState } from "../src/main/state.ts";
import type { NativeState } from "../src/shared/contract.ts";

function context(
  overrides: Partial<NativeState> = {},
  rest: Partial<MenuContext> = {},
): MenuContext {
  const viewer: ViewerState = {
    screen: { phase: "ready" },
    state: {
      ...blankState(),
      mode: "desktop",
      status: "connected",
      ready: true,
      ...overrides,
    },
  };
  return { viewer, hostScale: 2, hasGateway: true, idle: true, ...rest };
}

function launching(): MenuContext {
  return {
    viewer: { screen: { phase: "launching" }, state: blankState() },
    hostScale: 1,
    hasGateway: true,
    idle: true,
  };
}

function find(menus: ReturnType<typeof buildMenuSpec>, label: string) {
  return everyItem(menus).find((item) => item.label === label);
}

function menu(menus: ReturnType<typeof buildMenuSpec>, label: string) {
  const found = menus.find((m) => m.label === label);
  if (!found) {
    throw new Error(`no ${label} menu`);
  }
  return found;
}

describe("the shape of the bar", () => {
  test("it names the instance, so two of them are told apart", () => {
    const menus = buildMenuSpec(context({ branding: "work" }));
    expect(menus[0].label).toBe("work");
    expect(find(menus, "About work")).toBeDefined();
    expect(find(menus, "Quit work")).toBeDefined();
  });

  test("Edit keeps its standard roles", () => {
    // Without them ⌘C and ⌘V do nothing in a text field on macOS, and both the
    // clipboard panel and the configuration editor are text fields.
    const roles = menu(buildMenuSpec(context()), "Edit").items.map(
      (i) => i.role,
    );
    expect(roles).toContain("copy");
    expect(roles).toContain("paste");
    expect(roles).toContain("selectAll");
  });

  test("nothing outside App, Edit and Window carries a role that binds a chord", () => {
    // The whole keyboard design in one assertion: an accelerator on a Remote,
    // Display or View item would be a shortcut that does something else while a
    // desktop is focused. Roles are the only things in this tree that carry one,
    // and only three menus are allowed them.
    const menus = buildMenuSpec(context());
    const allowed = new Set([menus[0].label, "Edit", "Window"]);
    for (const spec of menus) {
      if (allowed.has(spec.label)) {
        continue;
      }
      expect(
        everyItem([spec])
          .filter((item) => item.role)
          .map((item) => item.label),
      ).toEqual([]);
    }
  });

  test("there is no Close item", () => {
    // ⌘W is one of the two chords this app exists to hand over. An item carrying it
    // would take it back the moment the desktop lost focus for an instant.
    expect(find(buildMenuSpec(context()), "Close")).toBeUndefined();
  });

  test("full screen is left to AppKit, which adds its own", () => {
    // It inserts *Enter Full Screen* into any menu titled "View", so an item here
    // is the second copy of it — which is what shipped, and what this stops.
    const view = menu(buildMenuSpec(context()), "View");
    expect(view.items.map((item) => item.role)).toEqual([
      undefined,
      undefined,
      undefined,
    ]);
    expect(
      find(buildMenuSpec(context()), "Toggle Full Screen"),
    ).toBeUndefined();
  });
});

describe("what the menus claim", () => {
  test("nothing is available on the launch screen", () => {
    const menus = buildMenuSpec(launching());
    for (const label of [
      "Clipboard…",
      "Enable Audio",
      "Refresh",
      "Switch Target",
      "Send Keys",
      "Resize to Window",
      "Resize to Display",
    ]) {
      expect(find(menus, label)?.enabled).toBe(false);
    }
    // Except the two that are the point of that screen.
    expect(find(menus, "Configuration…")?.enabled).toBe(true);
    expect(find(menus, "Restart Local Gateway")?.enabled).toBe(true);
  });

  test("a restart in progress disables the item that would start another", () => {
    const menus = buildMenuSpec(context({}, { idle: false }));
    expect(find(menus, "Restart Local Gateway")?.enabled).toBe(false);
  });

  test("a tick follows the report, and the command asks for the opposite", () => {
    // Nothing is set optimistically: the item asks for a change and the tick moves
    // when the page says it happened.
    const on = find(
      buildMenuSpec(context({ canAudio: true, audioEnabled: true })),
      "Enable Audio",
    );
    expect(on?.checked).toBe(true);
    expect(on?.command).toEqual({ type: "setAudio", enabled: false });

    const off = find(
      buildMenuSpec(context({ canAudio: true })),
      "Enable Audio",
    );
    expect(off?.checked).toBe(false);
    expect(off?.command).toEqual({ type: "setAudio", enabled: true });
  });

  test("Take Over is absent rather than greyed when it would mean nothing", () => {
    expect(find(buildMenuSpec(context()), "Take Over Session")).toBeUndefined();
    expect(
      find(buildMenuSpec(context({ status: "busy" })), "Take Over Session")
        ?.command,
    ).toEqual({ type: "takeOver" });
  });

  test("Send Keys offers the chords this keyboard cannot type", () => {
    const sendKeys = find(buildMenuSpec(context()), "Send Keys");
    const labels = sendKeys?.submenu?.map((item: MenuItemSpec) => item.label);
    expect(labels).toContain("Alt+F4");
    expect(labels).toContain("Tap Super");
    expect(sendKeys?.submenu?.[0].command).toEqual({
      type: "sendKeyCombo",
      codes: ["F5"],
    });
  });

  test("the Display menu says what there is, even when there is nothing to pick", () => {
    const empty = menu(buildMenuSpec(context()), "Display");
    expect(empty.items.at(-1)?.label).toBe("No Displays to Choose From");
    expect(empty.items.at(-1)?.enabled).toBe(false);

    const withDisplays = menu(
      buildMenuSpec(
        context({
          displays: [
            {
              id: 1,
              label: "Display 1",
              detail: "1920×1080",
              main: true,
              virtual: false,
            },
            {
              id: 2,
              label: "Display 2",
              detail: "1280×800",
              main: false,
              virtual: false,
            },
          ],
          activeDisplayId: 2,
        }),
      ),
      "Display",
    );
    const items = withDisplays.items.filter((item) => item.command);
    expect(items).toHaveLength(2);
    expect(items[1].checked).toBe(true);
    expect(items[1].command).toEqual({ type: "selectDisplay", id: 2 });
    // The readout is present for every target: on RDP and plain VNC these numbers
    // appear nowhere else at all.
    expect(withDisplays.items[0].enabled).toBe(false);
  });
});
