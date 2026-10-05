// A drag held past the edge between two virtual displays, each in a page of its
// own, crosses to the other: the position the page sends is let through towards
// the display shown beside it and held at every other edge.
//
// Everything asserted is a decision of the client or of the remote, not a pixel
// or a paint: the `mouseMove` the display socket carries while a button is held
// and the pointer is past the window — past the display's width towards the
// display beside it, and at zero upward, where nothing is — read off the socket
// as the page sends it. A browser hit-tests a held drag's moves, so past the
// window they target the document and not the desktop surface, which is what
// this guards. With `REMOTEX_PLAYWRIGHT_DRAG_MAC_SSH` set, the Mac is also asked
// where its pointer is while a drag is held past the edge: on the display
// beyond it. Whether a window dragged that way follows is left to a hand on a
// physical Mac; a virtual one applies the drag too late to judge it.
//
// It needs a gateway whose local config names a live host asked for two virtual
// displays, RDP or High Performance Mac:
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52890/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_DRAG_TARGET=win2 \
//     [REMOTEX_PLAYWRIGHT_DRAG_MAC_SSH=user@mac] \
//     bunx playwright test '/display-drag\.spec\.ts$'
import { execFileSync } from "node:child_process";
import { expect, type Page, test } from "@playwright/test";

import {
  BASE_URL,
  chooseDisplay,
  leaveSession,
  logInAndConnectTo,
  returnToPicker,
} from "./support";

/// The opt-in, and the target name in one.
const TARGET = process.env.REMOTEX_PLAYWRIGHT_DRAG_TARGET;
/// The Mac the target is, as `ssh` reaches it, for the window check; unset, the
/// socket is all that is read.
const MAC_SSH = process.env.REMOTEX_PLAYWRIGHT_DRAG_MAC_SSH;
/// How far past the window's edge the pointer is taken, in CSS pixels.
const PAST = 300;

/// What a page's display socket carried, by the display it named.
interface Traffic {
  /** The size each display was announced at, from its `resize`. */
  size: Map<number, { w: number; h: number }>;
  /** Every `mouseMove` the page sent, in order, by display. */
  moves: Map<number, { x: number; y: number }[]>;
  /** Every `mouseButton` the page sent, in order, by display. */
  buttons: Map<number, { button: string; pressed: boolean }[]>;
}

/// Watch every socket a page sends its input on, both ways: the session's page
/// sends on its session socket, `/ws`, which is the first display's, and a tab
/// on its display's socket. Registered before navigation.
function watchTraffic(page: Page): Traffic {
  const seen: Traffic = { size: new Map(), moves: new Map(), buttons: new Map() };
  page.on("websocket", (ws) => {
    const url = new URL(ws.url());
    if (url.pathname !== "/ws" && url.pathname !== "/ws/display") {
      return;
    }
    const display =
      url.pathname === "/ws" ? 1 : Number(url.searchParams.get("display"));
    const moves = seen.moves.get(display) ?? [];
    seen.moves.set(display, moves);
    const buttons = seen.buttons.get(display) ?? [];
    seen.buttons.set(display, buttons);
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      const message = JSON.parse(payload);
      if (message.type === "resize") {
        seen.size.set(display, { w: message.w, h: message.h });
      }
    });
    ws.on("framesent", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      const message = JSON.parse(payload);
      if (message.type === "mouseMove") {
        moves.push({ x: message.x, y: message.y });
      } else if (message.type === "mouseButton") {
        buttons.push({ button: message.button, pressed: message.pressed });
      }
    });
  });
  return seen;
}

/// The desktop surface's client rect, which the canvas shares on desktop.
async function surface(page: Page) {
  const box = await page.getByRole("application").boundingBox();
  if (!box) {
    throw new Error("the desktop surface has no box");
  }
  return box;
}

/// Press at the surface's centre and hold the pointer past the window at
/// `clientX`, then past the top as well; the positions sent meanwhile.
async function dragPast(
  page: Page,
  traffic: Traffic,
  display: number,
  clientX: number,
): Promise<{ held: { x: number; y: number }[]; released: boolean }> {
  const box = await surface(page);
  const moves = traffic.moves.get(display) ?? [];
  const buttons = traffic.buttons.get(display) ?? [];
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  const from = moves.length;
  await page.mouse.move(clientX, box.y + box.height / 2, { steps: 10 });
  await page.mouse.move(clientX, -PAST, { steps: 5 });
  // Motion is coalesced to one position a frame (outbound.ts), so what is
  // asserted is the newest, sent before the release is: a position past the
  // top, which is held at zero wherever the pointer is.
  await expect
    .poll(() => moves.length > from && moves[moves.length - 1].y === 0, {
      timeout: 5_000,
    })
    .toBe(true);
  const held = moves.slice(from);
  const releases = buttons.filter((b) => !b.pressed).length;
  await page.mouse.up();
  await expect
    .poll(() => buttons.filter((b) => !b.pressed).length, { timeout: 5_000 })
    .toBe(releases + 1);
  return { held, released: true };
}

/// Run `script` on the Mac with osascript, in `language`, and return its output.
function onMac(language: "AppleScript" | "JavaScript", script: string): string {
  return execFileSync(
    "ssh",
    ["-o", "BatchMode=yes", MAC_SSH ?? "", "osascript", "-l", language, "-e", `'${script}'`],
    { encoding: "utf8", timeout: 20_000 },
  ).trim();
}

/// The Mac's screens, each as `[x, y, w, h]` in its global coordinates, and
/// where its pointer is in them: AppKit's, with y up from the main screen's
/// bottom, so only x is compared here.
function macPointer(): { screens: number[][]; mouse: [number, number] } {
  return JSON.parse(
    onMac(
      "JavaScript",
      'ObjC.import("AppKit"); const s = $.NSScreen.screens; const out = []; ' +
        "for (let i = 0; i < s.count; i++) { const f = s.objectAtIndex(i).frame; " +
        "out.push([f.origin.x, f.origin.y, f.size.width, f.size.height]); } " +
        "const m = $.NSEvent.mouseLocation; JSON.stringify({ screens: out, mouse: [m.x, m.y] })",
    ),
  );
}

/// Press at the surface's centre and hold the pointer past the window at
/// `clientX`; the release, for after the Mac has been asked where its pointer is.
async function holdPast(page: Page, clientX: number): Promise<() => Promise<void>> {
  const box = await surface(page);
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(clientX, box.y + box.height / 2, { steps: 10 });
  return async () => {
    await page.mouse.up();
    await page.waitForTimeout(200);
  };
}

/// How long the Mac is given to act on input: a virtual one applies it seconds
/// late once its media streams are up.
const MAC_TIMEOUT_MS = 20_000;

test.describe("a drag held past the edge between two displays", () => {
  test.skip(
    !TARGET,
    "set REMOTEX_PLAYWRIGHT_DRAG_TARGET=<target> against a gateway with a live host asked for two virtual displays",
  );

  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("crosses to the display shown beside, and is held at every other edge", async ({
    page,
    context,
  }) => {
    test.setTimeout(120_000);
    const traffic = watchTraffic(page);
    await logInAndConnectTo(page, TARGET ?? "", "", { resize: true });
    await expect(page.locator("canvas").first()).toBeVisible({ timeout: 20_000 });
    await expect.poll(() => traffic.size.get(1), { timeout: 20_000 }).toBeDefined();
    const first = traffic.size.get(1);
    if (!first) {
      throw new Error("unreachable");
    }
    const viewport = page.viewportSize();
    if (!viewport) {
      throw new Error("no viewport");
    }

    // One display shown, nothing beside it: held at the display's edge both ways.
    // Chosen, since two displays start shown beside each other.
    await chooseDisplay(page, "All Displays", "Display 1");
    const alone = await dragPast(page, traffic, 1, viewport.width + PAST);
    const aloneLast = alone.held[alone.held.length - 1];
    expect(aloneLast).toEqual({ x: first.w - 1, y: 0 });
    expect(alone.held.every((m) => m.x <= first.w - 1 && m.y >= 0)).toBe(true);

    // All Displays: the second display is in a tab, to the right of this page's.
    await chooseDisplay(page, "Display 1", "All Displays");
    const tab = await context.newPage();
    const tabTraffic = watchTraffic(tab);
    await tab.goto(new URL("/display/2", BASE_URL).toString());
    await expect(tab.locator("canvas").first()).toBeVisible({ timeout: 20_000 });
    await expect(tab.getByRole("alert")).toHaveCount(0);
    await expect.poll(() => tabTraffic.size.get(2), { timeout: 20_000 }).toBeDefined();
    const second = tabTraffic.size.get(2);
    if (!second) {
      throw new Error("unreachable");
    }

    // From this page, past the right edge: through, towards the second display;
    // upward still held.
    const across = await dragPast(page, traffic, 1, viewport.width + PAST);
    const acrossLast = across.held[across.held.length - 1];
    expect(acrossLast.x, "past the edge towards the second display").toBeGreaterThan(
      first.w - 1,
    );
    expect(acrossLast.y, "held at the top, where nothing is shown").toBe(0);

    // From the tab, past the left edge: through, towards the first display.
    const back = await dragPast(tab, tabTraffic, 2, -PAST);
    const backLast = back.held[back.held.length - 1];
    expect(backLast.x, "past the edge towards the first display").toBeLessThan(0);
    expect(backLast.y).toBe(0);
    // And past its right edge, where nothing is shown: held.
    const beyond = await dragPast(tab, tabTraffic, 2, viewport.width + PAST);
    expect(beyond.held[beyond.held.length - 1]).toEqual({ x: second.w - 1, y: 0 });

    if (MAC_SSH) {
      // The Mac's own account. Its displays are the two, side by side, and a
      // pointer held past the edge from either page is on the other display.
      const screens = macPointer().screens.map(([x, , w]) => [x, w]);
      expect(screens).toEqual(
        expect.arrayContaining([
          [0, first.w],
          [first.w, second.w],
        ]),
      );
      let release = await holdPast(page, viewport.width + PAST);
      await expect
        .poll(() => macPointer().mouse[0], {
          message: "held past the right edge, the pointer is on the second display",
          timeout: MAC_TIMEOUT_MS,
        })
        .toBeGreaterThanOrEqual(first.w);
      await release();
      release = await holdPast(tab, -PAST);
      await expect
        .poll(() => macPointer().mouse[0], {
          message: "held past the left edge, the pointer is on the first display",
          timeout: MAC_TIMEOUT_MS,
        })
        .toBeLessThan(first.w);
      await release();
    }

    await tab.close();
    await returnToPicker(page);
  });
});
