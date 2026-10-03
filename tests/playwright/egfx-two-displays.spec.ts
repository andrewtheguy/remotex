// A session started with the graphics pipeline passed, over two virtual displays:
// the page that holds the session composes the span the host draws and shows one
// display of it, and the second display's tab is painted from the same picture,
// handed across the browser, where every other second display's tab is sent VP9
// cut from the gateway's framebuffer.
//
// Everything asserted is decided by the system and not by a machine's timing: the
// `graphicsView` each display socket carries beside its `resize`, which names the
// column of the picture that display is; that the second display's socket carries
// no picture of its own — no `videoFormat`, no binary frame; the DOM's account of
// the canvas each page shows its picture on; and the view that moves when the
// picker moves the session's page to the other display. Nothing here looks at a
// pixel.
//
// It needs a gateway whose local config names a live RDP host asked for two
// virtual displays, and ticks the passthrough under it at the picker:
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52890/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_EGFX_DISPLAYS_TARGET=win2 \
//     bunx playwright test '/egfx-two-displays\.spec\.ts$'
import { expect, type Page, test } from "@playwright/test";

import {
  BASE_URL,
  leaveSession,
  logInAndConnectTo,
  returnToPicker,
} from "./support";

/// The opt-in, and the target name in one.
const TARGET = process.env.REMOTEX_PLAYWRIGHT_EGFX_DISPLAYS_TARGET;

/// What every session here is started with: the pipeline passed, and nothing else.
const PASSED = { passthrough: true };

/// Everything a page's display sockets carried, by the display each one named.
interface Displays {
  /** Every control message's `type`, in arrival order, by display. */
  controlTypes: Map<number, string[]>;
  /** Each `graphicsView`, in arrival order, by display. */
  views: Map<number, { x: number; y: number; w: number; h: number }[]>;
  /** How many binary frames each display's sockets carried. */
  binary: Map<number, number>;
}

/// Watch every display socket a page opens. Registered before navigation.
function watchDisplays(page: Page): Displays {
  const seen: Displays = {
    controlTypes: new Map(),
    views: new Map(),
    binary: new Map(),
  };
  page.on("websocket", (ws) => {
    const url = new URL(ws.url());
    if (url.pathname !== "/ws/display") {
      return;
    }
    const display = Number(url.searchParams.get("display"));
    const types = seen.controlTypes.get(display) ?? [];
    seen.controlTypes.set(display, types);
    const views = seen.views.get(display) ?? [];
    seen.views.set(display, views);
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload !== "string") {
        seen.binary.set(display, (seen.binary.get(display) ?? 0) + 1);
        return;
      }
      const message = JSON.parse(payload);
      if (typeof message.type !== "string") {
        return;
      }
      types.push(message.type);
      if (message.type === "graphicsView") {
        views.push({ x: message.x, y: message.y, w: message.w, h: message.h });
      }
    });
  });
  return seen;
}

const last = <T>(items: T[] | undefined): T | undefined =>
  items?.[items.length - 1];

/// Choose `display` in the session page's Display picker, from the drawer's
/// button for the display shown now, and leave the drawer and the picker closed
/// whichever of them the choice left open.
async function chooseDisplay(page: Page, shown: string, display: string) {
  await page.getByRole("button", { name: "Open menu" }).click();
  await page.getByRole("button", { name: shown, exact: true }).click();
  // The picker's entries are pressed buttons, one pressed at a time; the one
  // asked for is not it yet.
  await page.getByRole("button", { name: display, pressed: false }).click();
  const picker = page.getByRole("button", { name: "Close display picker" });
  if (await picker.isVisible()) {
    await picker.click();
  }
  const drawer = page.getByRole("button", { name: "Close menu" });
  if (await drawer.isVisible()) {
    await drawer.click();
  }
}

test.describe("a target that passes its pipeline over two virtual displays", () => {
  test.skip(
    !TARGET,
    "set REMOTEX_PLAYWRIGHT_EGFX_DISPLAYS_TARGET=<target> against a gateway with a live RDP host asked for two virtual displays",
  );

  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("shows one display of the span, and paints the other's tab from the same picture", async ({
    page,
    context,
  }) => {
    // Two pages, each connecting and composing.
    test.setTimeout(90_000);
    const seen = watchDisplays(page);
    await logInAndConnectTo(page, TARGET ?? "", "", PASSED);
    // Composing: the pipeline's picture is on the canvas the page shows it on.
    await expect(page.locator("canvas.graphics")).toBeVisible({
      timeout: 20_000,
    });

    // The first display's socket names the column this page shows beside its
    // size: the first, from the picture's origin, and the size the desktop was
    // announced at.
    const first = seen.controlTypes.get(1) ?? [];
    expect(first.indexOf("resize")).toBeGreaterThanOrEqual(0);
    expect(first.indexOf("graphicsView")).toBeGreaterThan(first.indexOf("resize"));
    const shown = last(seen.views.get(1));
    expect(shown).toBeDefined();
    expect([shown?.x, shown?.y]).toEqual([0, 0]);
    expect(shown?.w).toBeGreaterThan(0);
    expect(shown?.h).toBeGreaterThan(0);

    // All Displays: the first display stays here and the second is a tab's.
    await chooseDisplay(page, "Display 1", "All Displays");

    // The second display's tab, in this browser: a page of the same context.
    const tab = await context.newPage();
    const tabSeen = watchDisplays(tab);
    await tab.goto(new URL("/display/2", BASE_URL).toString());
    // The tab asks before it takes the display, which one tab shows at a time.
    await tab.getByRole("button", { name: "Connect" }).click();
    // Painted from the session page's picture: the canvas it is shown on is up
    // once the paint worker has taken the first update across the browser.
    await expect(tab.locator("canvas.graphics")).toBeVisible({
      timeout: 20_000,
    });
    await expect(tab.getByRole("alert")).toHaveCount(0);

    // Its socket named the second column, beside its size, and carried no
    // picture of its own: nothing to decode in this tab.
    const second = tabSeen.controlTypes.get(2) ?? [];
    expect(second.indexOf("graphicsView")).toBeGreaterThan(second.indexOf("resize"));
    const column = last(tabSeen.views.get(2));
    expect(column?.x).toBe(shown?.w);
    expect(column?.y).toBe(0);
    expect([column?.w, column?.h]).toEqual([shown?.w, shown?.h]);
    expect(second, "a passed pipeline is no video stream").not.toContain("videoFormat");
    expect(tabSeen.binary.get(2) ?? 0, "binary frames on the second display's socket").toBe(0);
    expect(second, "a tab composes no pipeline").not.toContain("graphicsStart");

    // The session's page is still the first column, and still composing.
    expect(last(seen.views.get(1))?.x).toBe(0);
    await expect(page.getByRole("alert")).toHaveCount(0);

    // The picker moving this page to the second display moves the view across
    // the picture, and asks the host for nothing; the tab's display is no
    // longer shown in a tab, and its page says so.
    await chooseDisplay(page, "All Displays", "Display 2");
    await expect
      .poll(() => last(seen.views.get(1))?.x, { timeout: 10_000 })
      .toBe(shown?.w);
    expect(
      (seen.controlTypes.get(1) ?? []).filter((type) => type === "graphicsStart").length,
      "the pipeline is not started over for a switch of display",
    ).toBe(1);
    await expect(tab.getByText("Display not available")).toBeVisible({
      timeout: 10_000,
    });
    await tab.close();

    await returnToPicker(page);
  });
});
