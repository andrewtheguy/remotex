// The soft keyboard, read off the session socket: that a key tapped on it is the
// `key` frames the page sends, down then up; that a tapped modifier wraps the next
// key and is spent, and a twice-tapped one is off again; that a phone gets the
// docked keyboard, its strip, its shortcut row with its held modifiers and its
// Sym page, and that a shifted symbol there is Shift and the key.
//
// Everything asserted is a system decision: the frames the page sent, parsed by
// this file, and the accessible state of the keys. Nothing here is about repeat,
// which is timing, or about where the keys are drawn.
//
// It runs against whatever target the run is configured for: the keys go to the
// engine, and what the remote makes of them is not read here.
import { expect, type Page, test } from "@playwright/test";

import { leaveSession, logInAndConnect } from "./support";

interface Key {
  code: string;
  pressed: boolean;
}

const down = (code: string): Key => ({ code, pressed: true });
const up = (code: string): Key => ({ code, pressed: false });

/// Every `key` frame the page sends on the session socket, in order. Registered
/// before navigation so nothing is missed.
function watchKeys(page: Page): Key[] {
  const keys: Key[] = [];
  page.on("websocket", (ws) => {
    if (new URL(ws.url()).pathname !== "/ws") {
      return;
    }
    ws.on("framesent", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      const message: Record<string, unknown> = JSON.parse(payload);
      if (message.type === "key") {
        keys.push({
          code: message.code as string,
          pressed: message.pressed as boolean,
        });
      }
    });
  });
  return keys;
}

async function openKeyboard(page: Page): Promise<void> {
  await page.getByRole("button", { name: "Open menu" }).click();
  await page.getByRole("button", { name: "Soft keyboard", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Close soft keyboard" }),
  ).toBeVisible();
}

const key = (page: Page, name: string) =>
  page.getByRole("button", { name, exact: true });

test.describe("the soft keyboard", () => {
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("a key is sent down then up when it is clicked", async ({ page }) => {
    const keys = watchKeys(page);
    await logInAndConnect(page);
    await openKeyboard(page);

    await key(page, "a").click();
    await expect.poll(() => keys).toEqual([down("KeyA"), up("KeyA")]);
  });

  test("a tapped Shift wraps the next key and is spent; tapped twice it is off again", async ({
    page,
  }) => {
    const keys = watchKeys(page);
    await logInAndConnect(page);
    await openKeyboard(page);
    // The PC grid has a Shift on each side; the left one is first.
    const shift = key(page, "Shift").first();

    await shift.click();
    await expect(shift).toHaveAttribute("aria-pressed", "true");
    await key(page, "a").click();
    await expect.poll(() => keys).toEqual([
      down("ShiftLeft"),
      down("KeyA"),
      up("KeyA"),
      up("ShiftLeft"),
    ]);
    await expect(shift).toHaveAttribute("aria-pressed", "false");

    await key(page, "a").click();
    await expect.poll(() => keys.slice(4)).toEqual([down("KeyA"), up("KeyA")]);

    // Nothing locks: the second tap disarms it, and the key after is plain.
    await shift.click();
    await expect(shift).toHaveAttribute("aria-pressed", "true");
    await shift.click();
    await expect(shift).toHaveAttribute("aria-pressed", "false");
    await key(page, "s").click();
    await expect.poll(() => keys.slice(6)).toEqual([down("KeyS"), up("KeyS")]);
  });
});

test.describe("the soft keyboard on a phone", () => {
  // A phone as the page tells one: a screen whose short side is a phone's, and,
  // set in the test, the two touch points a pinch needs.
  test.use({
    viewport: { width: 390, height: 844 },
    contextOptions: { screen: { width: 390, height: 844 } },
    hasTouch: true,
    isMobile: true,
  });

  test.beforeEach(async ({ page }) => {
    await page.addInitScript(() => {
      Object.defineProperty(Navigator.prototype, "maxTouchPoints", {
        get: () => 5,
      });
    });
  });

  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("a phone gets the docked keyboard: strip, shortcut row and the Sym page", async ({
    page,
  }) => {
    const keys = watchKeys(page);
    await logInAndConnect(page);
    await openKeyboard(page);

    // The strip's arrows and modifiers are on the first page.
    await expect(key(page, "Left")).toBeVisible();
    await key(page, "a").tap();
    await expect.poll(() => keys).toEqual([down("KeyA"), up("KeyA")]);

    // A tapped Ctrl wraps the next key.
    await key(page, "Ctrl").tap();
    await key(page, "c").tap();
    await expect.poll(() => keys.slice(2)).toEqual([
      down("ControlLeft"),
      down("KeyC"),
      up("KeyC"),
      up("ControlLeft"),
    ]);

    // The shortcut row's combos are sent as the combo.
    await key(page, "Alt+Tab").tap();
    await expect.poll(() => keys.slice(6)).toEqual([
      down("AltLeft"),
      down("Tab"),
      up("Tab"),
      up("AltLeft"),
    ]);

    // The shortcut row's held modifiers are the key itself, down for as long
    // as it is touched: a tap on one is a bare press of it, and it arms nothing.
    const heldSuper = key(page, "Hold Super");
    await heldSuper.tap();
    await expect.poll(() => keys.slice(10)).toEqual([
      down("MetaLeft"),
      up("MetaLeft"),
    ]);
    await expect(heldSuper).toHaveAttribute("aria-pressed", "false");
    await expect(key(page, "Super")).toHaveAttribute("aria-pressed", "false");

    // The Sym page: a shifted symbol is Shift and its key, and the strip stays.
    await key(page, "?123").tap();
    await expect(key(page, "ABC")).toBeVisible();
    await expect(key(page, "Left")).toBeVisible();
    await key(page, "_").tap();
    await expect.poll(() => keys.slice(12)).toEqual([
      down("ShiftLeft"),
      down("Minus"),
      up("Minus"),
      up("ShiftLeft"),
    ]);
    await key(page, "ABC").tap();
    await expect(key(page, "?123")).toBeVisible();
  });
});
