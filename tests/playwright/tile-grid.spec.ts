// The `render_grid_debug` lattice: the one render debug aid the browser draws
// rather than receives. Nothing here looks at a pixel — what is asserted is that
// the gateway states the switch on `connected` and the pitch on every `resize`,
// at 64 points of the framebuffer's density, and that the overlay canvas the
// client answers with is exactly the framebuffer canvas's twin, which is the whole
// of the claim. Whether the dashes are visible is the operator's eyes; whether the
// overlay is the right size and in the right place is a decision, and decisions
// are what belongs here.
//
// It needs a gateway whose local config has a target with `render_grid_debug =
// true`. Keep that gitignored file under `tmp/`, for example `tmp/qa_grid.toml`:
//
//     cargo run -- serve --config tmp/qa_grid.toml
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52888/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_GRID_TARGET=gridtiles \
//     REMOTEX_PLAYWRIGHT_TARGET=workstationlinux \
//     npx playwright test tile-grid
import { expect, type Page, test } from "@playwright/test";

import { leaveSession, logInAndConnectTo, returnToPicker, TARGET } from "./support";

/// The opt-in, and the target name in one — the same bargain the video and audio
/// specs make. Its presence is the claim that this gateway has a target with the
/// lattice on; without one the spec would assert against an overlay that is
/// correctly empty, and pass for the wrong reason.
const GRID_TARGET = process.env.REMOTEX_PLAYWRIGHT_GRID_TARGET;

/// `protocol::CELL_POINTS` and the `TileGrid::at` rule, copied rather than
/// imported: this spec is the check that the gateway states the grid it actually
/// cuts damage at, and reading the client's own copy of the number to decide that
/// would be asking the accused. 64 points, so 64 pixels under the 1.5 midpoint
/// and 128 from it up.
const CELL_POINTS = 64;
function pitchAt(scale: number): { w: number; h: number } {
  const pixels = CELL_POINTS * (scale >= 1.5 ? 2 : 1);
  return { w: pixels, h: pixels };
}

/// The `connected` and `resize` control messages for the session this page ends
/// up in, collected from the socket rather than from the SPA's state — the
/// control-plane JSON is the thing under test. Registered before the page is
/// opened, because the socket is opened by the login and a listener attached
/// afterwards would miss the first session.
function controlMessages(page: Page): {
  connected: Array<Record<string, unknown>>;
  resizes: Array<{ scale: number; tileGrid: { w: number; h: number } }>;
} {
  const connected: Array<Record<string, unknown>> = [];
  const resizes: Array<{ scale: number; tileGrid: { w: number; h: number } }> = [];
  page.on("websocket", (ws) => {
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload !== "string" || !payload.startsWith("{")) {
        return;
      }
      const msg = JSON.parse(payload) as Record<string, unknown>;
      if (msg.type === "connected") {
        connected.push(msg);
      } else if (msg.type === "resize") {
        resizes.push(msg as { scale: number; tileGrid: { w: number; h: number } });
      }
    });
  });
  return { connected, resizes };
}

/// The two canvases as the DOM has them: bitmap dimensions and the CSS box each
/// one occupies. Read together in one evaluate so both describe the same layout.
async function canvases(page: Page) {
  return await page.evaluate(() => {
    const read = (selector: string) => {
      const el = document.querySelector(selector);
      if (!(el instanceof HTMLCanvasElement)) {
        return null;
      }
      const { x, y, width, height } = el.getBoundingClientRect();
      return {
        bitmap: { w: el.width, h: el.height },
        box: { x, y, width, height },
        pointerEvents: getComputedStyle(el).pointerEvents,
      };
    };
    return { framebuffer: read("canvas.framebuffer"), grid: read("canvas.tile-grid") };
  });
}

test.describe("the render_grid_debug lattice", () => {
  test.skip(
    !GRID_TARGET,
    "set REMOTEX_PLAYWRIGHT_GRID_TARGET to a target with render_grid_debug = true",
  );

  // Cleanup, so it runs even when an assertion above threw: see `leaveSession`.
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("a grid target states its lattice and gets an overlay the size of the desktop", async ({
    page,
  }) => {
    const { connected, resizes } = controlMessages(page);
    await logInAndConnectTo(page, GRID_TARGET as string);

    // The gateway's half: the switch on `connected`, and on every `resize` the
    // pitch itself rather than a bare flag, so the client cannot draw a grid that
    // merely looks like the one damage was cut at. The pitch is 64 points of the
    // density the same message announces, whatever that density turned out to be.
    await expect.poll(() => connected.at(-1)?.gridDebug).toBe(true);
    await expect.poll(() => resizes.length).toBeGreaterThan(0);
    for (const resize of resizes) {
      expect(resize.tileGrid).toEqual(pitchAt(resize.scale));
    }

    // The client's half, asserted as the *relationship* between the two canvases
    // rather than as a size: a desktop is whatever this window asked the remote
    // for, and the overlay's whole job is to be the same as it. Polled until the
    // first frame has given the desktop a bitmap at all, since a 0x0 pair would
    // agree with itself and mean nothing.
    await expect
      .poll(async () => (await canvases(page)).framebuffer?.bitmap.w ?? 0)
      .toBeGreaterThan(0);
    const { framebuffer, grid } = await canvases(page);
    expect(grid?.bitmap).toEqual(framebuffer?.bitmap);
    // The same CSS box, which is what makes a line drawn on a cell boundary land
    // on that boundary: the overlay holds framebuffer pixels like the desktop
    // canvas does, so any difference in the box is a lattice pointing at the
    // wrong columns.
    expect(grid?.box).toEqual(framebuffer?.box);
    // And it is scenery: the input overlay above it owns the pointer, and a
    // lattice that swallowed a click would break the desktop it is drawn over.
    expect(grid?.pointerEvents).toBe("none");
  });

  test("an ordinary target draws no lattice, and leaves none behind", async ({
    page,
  }) => {
    const { connected } = controlMessages(page);
    // Through the grid target first, so this also covers the switch: the overlay
    // is cleared by the session that does not want it rather than surviving into
    // it from the session that did.
    await logInAndConnectTo(page, GRID_TARGET as string);
    await expect.poll(() => connected.at(-1)?.gridDebug).toBe(true);
    await returnToPicker(page);

    await logInAndConnectTo(page, TARGET);
    await expect.poll(() => connected.at(-1)?.gridDebug).toBe(false);
    await expect
      .poll(async () => (await canvases(page)).grid?.bitmap)
      .toEqual({ w: 0, h: 0 });
  });
});
