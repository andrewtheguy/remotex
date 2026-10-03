// BETA: the software HEVC decoder (frontend/src/hevcWasmDecoder.ts), which
// a gateway that has its release archive serves at /hevc/ and which the page
// takes, under `?hevc_decoder=software`, for a High Performance Mac's passed stream.
//
// What is asserted is what the system decides: whether the page is cross-origin
// isolated and the decoder is served, what the page tells the gateway it decodes,
// what the gateway then announces, which files the page loads, and — for the one
// claim about decoding itself — that the first passed keyframe's batch was
// acknowledged with no decoder failure before it. That last holds by ordering, not
// timing: a failed decoder settles its unit and reports the failure in the same
// turn, and the paint worker posts the report before the acknowledgement, so the
// page has its banner up and has sent any repaint request before the gateway reads
// the ack (framePainter.ts, useRemoteDesktop.ts).
//
// It needs a gateway whose local config has an `ard-high-performance` target, which
// only a live Mac serves, and the release archive beside the config. The session is
// started with the Mac's stream passed, where the picker offers that:
//
//     cargo run --profile qa -- serve --config tmp/qa_hevc.toml
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52889/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_HEVC_TARGET=macvmhevc \
//     bun run test:hevc
//
// Against a gateway without the archive, set REMOTEX_PLAYWRIGHT_HEVC_WASM=0: the
// same page must then find no decoder and take VP9 and Opus.
import { expect, type Page, test } from "@playwright/test";

import { leaveSession, logInAndConnectTo } from "./support";

/// The opt-in, and the target name in one, as the video spec's.
const HEVC_TARGET = process.env.REMOTEX_PLAYWRIGHT_HEVC_TARGET;

/// Whether the gateway under test serves the decoder: said by whoever configured
/// it, because asking the gateway would let one that lost its decoder pass as one
/// configured without.
const SERVES_DECODER = process.env.REMOTEX_PLAYWRIGHT_HEVC_WASM !== "0";

const SOFTWARE = "?hevc_decoder=software";

/// The wire, copied from src/protocol.rs rather than imported from the SPA.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_VIDEO = 0x03;
const VIDEO_KEYFRAME = 0x01;

interface Session {
  /** The session socket's `apple_media`, the page's answer. */
  appleMedia?: string;
  formats: { decode: string; passthrough: boolean }[];
  /** The sequence of the first batch that opens with a keyframe after a passed format. */
  passedKeyframe?: number;
  /** Every `paintAck` sequence the page sent. */
  acks: number[];
  /** `refresh` requests the page sent, and after how many acks. */
  refreshes: number[];
  /** Paths under /hevc/ the page or its workers asked for, with method and status. */
  decoderFiles: string[];
}

/// Watch the session socket, the display socket that carries its picture, and the
/// decoder's files. Registered before navigation.
function watchSession(page: Page): Session {
  const seen: Session = { formats: [], acks: [], refreshes: [], decoderFiles: [] };
  // The context's, not the page's: the decoder's files are fetched by the paint
  // worker's decode worker and its threads, not by the page.
  page.context().on("response", (response) => {
    const url = new URL(response.url());
    if (url.pathname.startsWith("/hevc/")) {
      seen.decoderFiles.push(
        `${response.request().method()} ${url.pathname} ${response.status()}`,
      );
    }
  });
  page.on("websocket", (ws) => {
    const url = new URL(ws.url());
    if (url.pathname === "/ws") {
      seen.appleMedia = url.searchParams.get("apple_media") ?? undefined;
    } else if (url.pathname !== "/ws/display") {
      return;
    }
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload === "string") {
        const message = JSON.parse(payload);
        if (message.type === "videoFormat") {
          seen.formats.push({
            decode: message.decode,
            passthrough: message.passthrough === true,
          });
        }
        return;
      }
      if (payload.readUInt8(0) !== BATCH_FRAME_KIND) {
        return;
      }
      const passed = seen.formats.at(-1)?.passthrough === true;
      if (
        passed &&
        seen.passedKeyframe === undefined &&
        payload.length > BATCH_HEADER_LEN + 1 &&
        payload.readUInt8(BATCH_HEADER_LEN) === OP_VIDEO &&
        (payload.readUInt8(BATCH_HEADER_LEN + 1) & VIDEO_KEYFRAME) !== 0
      ) {
        seen.passedKeyframe = payload.readUInt32LE(4);
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

test.describe("a High Performance target under ?hevc_decoder=software", () => {
  test.skip(
    !HEVC_TARGET,
    "set REMOTEX_PLAYWRIGHT_HEVC_TARGET=<target> against a gateway with an ard-high-performance target",
  );
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("with the decoder served, the passed stream is decoded in software", async ({
    page,
  }) => {
    test.skip(!SERVES_DECODER, "the gateway has no decoder archive");
    const seen = watchSession(page);
    await logInAndConnectTo(page, HEVC_TARGET ?? "", SOFTWARE, {
      passthrough: true,
    });

    expect(await page.evaluate(() => globalThis.crossOriginIsolated)).toBe(true);
    expect(seen.appleMedia, "the page said it decodes the Mac's stream").toBe(
      "true",
    );
    await expect
      .poll(() => seen.formats.some((f) => f.passthrough), { timeout: 20_000 })
      .toBe(true);
    for (const format of seen.formats.filter((f) => f.passthrough)) {
      expect(format.decode).toMatch(/^hev1\./);
    }

    // The first passed keyframe, acknowledged: decoded, or failed and said so first.
    await expect
      .poll(() => seen.passedKeyframe, { timeout: 20_000 })
      .toBeDefined();
    const keyframe = seen.passedKeyframe ?? 0;
    await expect
      .poll(() => seen.acks.some((sequence) => sequence >= keyframe), {
        timeout: 20_000,
      })
      .toBe(true);
    await expect(page.getByRole("alert")).toHaveCount(0);
    expect(seen.refreshes, "repaints the page asked for").toEqual([]);
    // Its pictures are drawn on the canvas over the desktop's, which the page
    // shows from the first one (hevcPicture.ts).
    await expect(page.locator("canvas.graphics")).toBeVisible();

    // Asked for with a HEAD before choosing it, then loaded by the decode worker.
    expect(seen.decoderFiles).toContain("HEAD /hevc/hevc.wasm 200");
    expect(seen.decoderFiles).toContain("GET /hevc/hevc.js 200");
    expect(seen.decoderFiles).toContain("GET /hevc/hevc.wasm 200");
  });

  test("without the decoder, the page takes VP9 and Opus", async ({ page }) => {
    test.skip(SERVES_DECODER, "the gateway has the decoder archive");
    const seen = watchSession(page);
    // The passthrough is greyed at the picker for a page that cannot decode the
    // stream, so the session starts without it.
    await logInAndConnectTo(page, HEVC_TARGET ?? "", SOFTWARE);

    // Isolated as every page is, so the page asks, and is told no.
    expect(await page.evaluate(() => globalThis.crossOriginIsolated)).toBe(true);
    expect(seen.appleMedia, "asked for software where none is served").toBe(
      "false",
    );
    await expect
      .poll(() => seen.formats.length, { timeout: 20_000 })
      .toBeGreaterThan(0);
    for (const format of seen.formats) {
      expect(format.passthrough).toBe(false);
      expect(format.decode).toMatch(/^vp09\./);
    }
    expect(seen.decoderFiles).toEqual(["HEAD /hevc/hevc.wasm 404"]);
  });
});
