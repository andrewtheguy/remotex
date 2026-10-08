// BETA: the page's software VP9 decoder (frontend/src/softwareDecoder.ts), the
// bundled vp9-wasm module, which a page takes for the gateway's 4:4:4 stream
// where the gateway's `[vp9_wasm]` allows it: under `?vp9_decoder=software` here,
// since the browser these specs run has a decoder of its own for profile 1.
//
// What is asserted is what the system decides: what the gateway's config says, the
// chroma the page asks for on its session socket, what the gateway then announces,
// whether the page loads the module, which is only ever fetched to decode with,
// and — for the one claim about decoding itself — that the first keyframe's batch
// was acknowledged with no decoder failure before it. That last holds by ordering,
// not timing: a failed decoder settles its unit and reports the failure in the
// same turn, and the paint worker posts the report before the acknowledgement
// (framePainter.ts, useRemoteDesktop.ts). Which canvas the page shows says which
// path presented: the module's planes are drawn on the one over the desktop's.
//
// It needs a gateway whose local config has a live target that sends VP9 at the
// chroma the page asks for (no `render_chroma`), and `[vp9_wasm] enabled = true`:
//
//     cargo run --profile qa -- serve --config tmp/qa_vp9.toml
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52893/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_VP9_TARGET=desktop \
//     bun run test:vp9
//
// Against a gateway without the table, set REMOTEX_PLAYWRIGHT_VP9_WASM=0: the same
// page must then leave the module alone.
import { expect, type Page, test } from "@playwright/test";

import { BASE_URL, leaveSession, logInAndConnectTo } from "./support";

/// The opt-in, and the target name in one, as the video spec's.
const VP9_TARGET = process.env.REMOTEX_PLAYWRIGHT_VP9_TARGET;

/// Whether the gateway under test allows the module: said by whoever configured
/// it, and held against what the gateway itself says.
const ALLOWED = process.env.REMOTEX_PLAYWRIGHT_VP9_WASM !== "0";

const SOFTWARE = "?vp9_decoder=software";

/// The module in the bundle, under the name the build gives it.
const MODULE = /^\/assets\/vp9_bg-[\w-]+\.wasm$/;

/// The wire, copied from src/protocol.rs rather than imported from the SPA.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_VIDEO = 0x03;
const VIDEO_KEYFRAME = 0x01;

interface Session {
  /** The session socket's `chroma`, the page's answer. */
  chroma?: string;
  formats: string[];
  /** The sequence of the first batch that opens with a keyframe after a format. */
  keyframe?: number;
  /** Every `paintAck` sequence the page sent. */
  acks: number[];
  /** `refresh` requests the page sent, and after how many acks. */
  refreshes: number[];
  /** The module's file, as often as the page or its workers asked for it, with status. */
  moduleLoads: number[];
}

/// Watch the session socket, the display socket that carries its picture, and the
/// module's file. Registered before navigation.
function watchSession(page: Page): Session {
  const seen: Session = { formats: [], acks: [], refreshes: [], moduleLoads: [] };
  // The context's, not the page's: the module is fetched by the paint worker's
  // decode worker, not by the page.
  page.context().on("response", (response) => {
    if (MODULE.test(new URL(response.url()).pathname)) {
      seen.moduleLoads.push(response.status());
    }
  });
  page.on("websocket", (ws) => {
    const url = new URL(ws.url());
    if (url.pathname === "/ws") {
      seen.chroma = url.searchParams.get("chroma") ?? undefined;
    } else if (url.pathname !== "/ws/display") {
      return;
    }
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload === "string") {
        const message = JSON.parse(payload);
        if (message.type === "videoFormat") {
          seen.formats.push(message.decode);
        }
        return;
      }
      if (payload.readUInt8(0) !== BATCH_FRAME_KIND) {
        return;
      }
      if (
        seen.formats.length > 0 &&
        seen.keyframe === undefined &&
        payload.length > BATCH_HEADER_LEN + 1 &&
        payload.readUInt8(BATCH_HEADER_LEN) === OP_VIDEO &&
        (payload.readUInt8(BATCH_HEADER_LEN + 1) & VIDEO_KEYFRAME) !== 0
      ) {
        seen.keyframe = payload.readUInt32LE(4);
      }
    });
    ws.on("framesent", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      const message = JSON.parse(payload);
      if (message.type === "paintAck") {
        seen.acks.push(message.sequence);
      } else if (message.type === "refresh") {
        seen.refreshes.push(seen.acks.length);
      }
    });
  });
  return seen;
}

/// The stream's first keyframe, acknowledged: decoded, or failed and said so first.
async function firstKeyframeAcknowledged(page: Page, seen: Session) {
  await expect.poll(() => seen.keyframe, { timeout: 20_000 }).toBeDefined();
  const keyframe = seen.keyframe ?? 0;
  await expect
    .poll(() => seen.acks.some((sequence) => sequence >= keyframe), {
      timeout: 20_000,
    })
    .toBe(true);
  await expect(page.getByRole("alert")).toHaveCount(0);
  expect(seen.refreshes, "repaints the page asked for").toEqual([]);
}

/// What the gateway says of `[vp9_wasm]`, asked of it directly.
async function gatewayAllows(page: Page): Promise<unknown> {
  const response = await page.request.get(new URL("/api/config", BASE_URL).toString());
  return (await response.json()).vp9Wasm;
}

test.describe("a VP9 target and the page's software decoder", () => {
  test.skip(
    !VP9_TARGET,
    "set REMOTEX_PLAYWRIGHT_VP9_TARGET=<target> against a gateway with a live target",
  );
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("allowed and asked for, 4:4:4 is decoded in the module", async ({
    page,
  }) => {
    test.skip(!ALLOWED, "the gateway does not set [vp9_wasm]");
    expect(await gatewayAllows(page)).toBe(true);
    const seen = watchSession(page);
    await logInAndConnectTo(page, VP9_TARGET ?? "", SOFTWARE);

    expect(await page.evaluate(() => globalThis.crossOriginIsolated)).toBe(true);
    expect(seen.chroma, "the page asked for 4:4:4").toBe("444");
    await expect
      .poll(() => seen.formats.length, { timeout: 20_000 })
      .toBeGreaterThan(0);
    for (const decode of seen.formats) {
      expect(decode, "profile 1, which the module decodes").toMatch(/^vp09\.01\./);
    }

    await firstKeyframeAcknowledged(page, seen);
    // Its pictures are drawn on the canvas over the desktop's, which the page
    // shows from the first one (planesPicture.ts).
    await expect(page.locator("canvas.graphics")).toBeVisible();
    // Loaded by the decode worker and compiled once: its threads are given the
    // compiled module.
    expect(seen.moduleLoads).toEqual([200]);
  });

  test("allowed and not asked for, the browser's own decoder keeps 4:4:4", async ({
    page,
  }) => {
    test.skip(!ALLOWED, "the gateway does not set [vp9_wasm]");
    const seen = watchSession(page);
    await logInAndConnectTo(page, VP9_TARGET ?? "");

    // Chromium decodes profile 1 itself, so the module is not this page's.
    expect(seen.chroma).toBe("444");
    await firstKeyframeAcknowledged(page, seen);
    await expect(page.locator("canvas.graphics")).toBeHidden();
    expect(seen.moduleLoads).toEqual([]);
  });

  test("not allowed, the switch in the URL fails the stream by name", async ({
    page,
  }) => {
    test.skip(ALLOWED, "the gateway sets [vp9_wasm]");
    expect(await gatewayAllows(page)).toBe(false);
    const seen = watchSession(page);
    await logInAndConnectTo(page, VP9_TARGET ?? "", SOFTWARE);

    // Not the browser's own decoder in the module's place: the page says what
    // was asked for and that the gateway does not enable it.
    await expect(page.getByRole("alert")).toContainText(
      "this gateway does not enable ([vp9_wasm])",
    );
    expect(seen.chroma).toBe("444");
    await expect(page.locator("canvas.graphics")).toBeHidden();
    expect(seen.moduleLoads).toEqual([]);
  });
});
