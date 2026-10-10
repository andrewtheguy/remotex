// The page's software VP9 decoder (frontend/src/softwareDecoder.ts), the bundled
// vp9-wasm module, which decodes the gateway's 4:4:4 stream in a page whose
// browser's own decoder does not take profile 1 (frontend/src/nativeVp9.ts). The
// gateway sends every browser the same stream and is told nothing: which decoder
// a page builds is that page's own decision.
//
// What is asserted is what the system decides: that the picker has no row for it,
// what the gateway announces — profile 1, never said to be the page's — whether
// the page loads the module, which is only ever fetched to decode with, and — for
// the one claim about decoding itself — that the first keyframe's batch was
// acknowledged with no decoder failure before it. That last holds by ordering,
// not timing: a failed decoder settles its unit and reports the failure in the
// same turn, and the paint worker posts the report before the acknowledgement
// (framePainter.ts, useRemoteDesktop.ts). Which canvas the page shows says which
// path presented: the module's planes are drawn on the one over the desktop's.
// Playwright's Chromium decodes profile 1 itself, so a browser that does not is
// made by answering the page's one question of it with a no.
//
// It needs a gateway whose local config has a live target that sends VP9:
//
//     cargo run --profile qa -- serve --config tmp/qa_vp9.toml
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52893/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_VP9_TARGET=desktop \
//     bun run test:vp9
import { expect, type Page, test } from "@playwright/test";

import {
  leaveSession,
  logIn,
  logInAndConnectTo,
  returnToPicker,
  targetNamePattern,
} from "./support";

/// The opt-in, and the target name in one, as the video spec's.
const VP9_TARGET = process.env.REMOTEX_PLAYWRIGHT_VP9_TARGET;

/// The module in the bundle, under the name the build gives it.
const MODULE = /^\/assets\/vp9_bg-[\w-]+\.wasm$/;

/// A picker row for decoding in the page, by its label: only a Mac's HEVC has one.
const ROW = /^Decode .* in this page/;

/// The wire, copied from src/protocol.rs rather than imported from the SPA.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_VIDEO = 0x03;
const VIDEO_KEYFRAME = 0x01;

interface Format {
  decode: string;
  software: unknown;
}

interface Session {
  /** Each session socket's query, in the order opened. */
  queries: URLSearchParams[];
  /** Each display socket's `videoFormat`s, in the order the sockets opened. */
  sockets: Format[][];
  /** The sequence of the first batch that opens with a keyframe on the newest socket. */
  keyframe?: number;
  /** Every `paintAck` sequence the page sent on the newest socket. */
  acks: number[];
  /** `refresh` requests the page sent, and after how many acks. */
  refreshes: number[];
  /** The module's file, as often as the page or its workers asked for it, with status. */
  moduleLoads: number[];
}

/// Watch the session socket, the display socket that carries its picture, and the
/// module's file. Registered before navigation.
function watchSession(page: Page): Session {
  const seen: Session = {
    queries: [],
    sockets: [],
    acks: [],
    refreshes: [],
    moduleLoads: [],
  };
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
      seen.queries.push(url.searchParams);
      return;
    }
    if (url.pathname !== "/ws/display") {
      return;
    }
    const formats: Format[] = [];
    seen.sockets.push(formats);
    seen.keyframe = undefined;
    seen.acks = [];
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload === "string") {
        const message = JSON.parse(payload);
        if (message.type === "videoFormat") {
          formats.push({ decode: message.decode, software: message.software });
        }
        return;
      }
      if (payload.readUInt8(0) !== BATCH_FRAME_KIND) {
        return;
      }
      if (
        formats.length > 0 &&
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

/// Make the page's browser one whose own decoder refuses profile 1, as far as the
/// page's one question of it goes.
async function refuseProfile1(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const ask = VideoDecoder.isConfigSupported.bind(VideoDecoder);
    VideoDecoder.isConfigSupported = (config) =>
      config.codec.startsWith("vp09.01.")
        ? Promise.resolve({ supported: false, config })
        : ask(config);
  });
}

/// The newest display socket's first keyframe, acknowledged: decoded, or failed
/// and said so first.
async function firstKeyframeAcknowledged(page: Page, seen: Session) {
  await expect.poll(() => seen.keyframe, { timeout: 20_000 }).toBeDefined();
  await expect
    .poll(
      () => seen.acks.some((sequence) => sequence >= (seen.keyframe ?? 0)),
      { timeout: 20_000 },
    )
    .toBe(true);
  await expect(page.getByRole("alert")).toHaveCount(0);
}

/// Every format the newest display socket announced, once it has announced one:
/// profile 1, and none of them said to be the page's to decode.
async function announcedProfile1(seen: Session, sockets: number) {
  await expect
    .poll(() => seen.sockets.length >= sockets && seen.sockets.at(-1)?.length, {
      timeout: 20_000,
    })
    .toBeTruthy();
  for (const format of seen.sockets.at(-1) ?? []) {
    expect(format.decode, "the gateway's 4:4:4").toMatch(/^vp09\.01\./);
    expect(format.software, "the page's own decision").toBe(false);
  }
}

/// The Info card's Video row ends with which decoder the session's picture has.
async function expectInfo(page: Page, decoder: string): Promise<void> {
  await page.getByRole("button", { name: "Open menu" }).click();
  await page.getByRole("button", { name: "Info", exact: true }).click();
  const card = page.getByRole("dialog", { name: "Info" });
  await expect(card).toContainText(decoder);
  await page.keyboard.press("Escape");
  await expect(card).toHaveCount(0);
  const close = page.getByRole("button", { name: "Close menu" });
  if (await close.isVisible()) {
    await close.click();
  }
}

test.describe("a VP9 target and the page's software decoder", () => {
  test.skip(
    !VP9_TARGET,
    "set REMOTEX_PLAYWRIGHT_VP9_TARGET=<target> against a gateway with a live target",
  );
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("the picker offers no choice of VP9 decoder", async ({ page }) => {
    await logIn(page);
    if (await page.getByRole("button", { name: "Open menu" }).isVisible()) {
      await returnToPicker(page);
    }
    const row = page.getByRole("button", {
      name: targetNamePattern(VP9_TARGET ?? ""),
    });
    await row.click();
    const item = page.getByRole("listitem").filter({ has: row });
    await expect(
      item.getByRole("button", { name: "Start", exact: true }),
    ).toBeVisible();
    await expect(item.getByRole("checkbox", { name: ROW })).toHaveCount(0);
  });

  test("a browser whose own decoder takes profile 1 decodes it itself", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, VP9_TARGET ?? "");

    expect(
      seen.queries.at(-1)?.has("chroma"),
      "the gateway is asked nothing about VP9",
    ).toBe(false);
    await announcedProfile1(seen, 1);
    await firstKeyframeAcknowledged(page, seen);
    await expect(page.locator("canvas.graphics")).toBeHidden();
    expect(seen.moduleLoads).toEqual([]);
    await expectInfo(page, "decoded by the browser's native decoder");
  });

  test("a browser whose own decoder refuses profile 1 is sent the same stream and decodes it in the module, and again when it comes back", async ({
    page,
  }) => {
    await refuseProfile1(page);
    const seen = watchSession(page);
    await logInAndConnectTo(page, VP9_TARGET ?? "");

    expect(await page.evaluate(() => globalThis.crossOriginIsolated)).toBe(true);
    await announcedProfile1(seen, 1);
    await firstKeyframeAcknowledged(page, seen);
    expect(seen.refreshes, "repaints the page asked for").toEqual([]);
    // Its pictures are drawn on the canvas over the desktop's, which the page
    // shows from the first one (planesPicture.ts).
    await expect(page.locator("canvas.graphics")).toBeVisible();
    // Loaded by the decode worker and compiled once: its threads are given the
    // compiled module.
    expect(seen.moduleLoads).toEqual([200]);
    await expectInfo(page, "decoded by this page's WebAssembly decoder");

    // The page that comes back asks again, and decodes the same stream the same way.
    await page.reload();
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });
    await announcedProfile1(seen, 2);
    await firstKeyframeAcknowledged(page, seen);
    await expect(page.locator("canvas.graphics")).toBeVisible();
    expect(seen.moduleLoads).toHaveLength(2);
  });
});
