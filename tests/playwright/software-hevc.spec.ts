// BETA: the software HEVC decoder (frontend/src/softwareDecoder.ts), which a
// gateway that has its release archive serves at /hevc/ and which decodes a High
// Performance Mac's passed stream in a session started with "Decode in this page"
// at the picker.
//
// What is asserted is what the system decides: what the gateway lists the target
// as offering, what Start sends, what the gateway then announces — the passed
// stream, and that this page decodes it — which files the page loads, and — for
// the one claim about decoding itself — that the first passed keyframe's batch was
// acknowledged with no decoder failure before it. That last holds by ordering, not
// timing: a failed decoder settles its unit and reports the failure in the same
// turn, and the paint worker posts the report before the acknowledgement, so the
// page has its banner up and has sent any repaint request before the gateway reads
// the ack (framePainter.ts, useRemoteDesktop.ts).
//
// It needs a gateway whose local config has an `ard-high-performance` target, which
// only a live Mac serves, and the release archive beside the config or named by
// `[hevc_wasm]`:
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
// same target must then offer no such decoder, and a browser whose own decoder
// refuses the Mac's HEVC, as Playwright's Chromium does, takes VP9.
import { expect, type Page, test } from "@playwright/test";

import { BASE_URL, leaveSession, logInAndConnectTo } from "./support";

/// The opt-in, and the target name in one, as the video spec's.
const HEVC_TARGET = process.env.REMOTEX_PLAYWRIGHT_HEVC_TARGET;

/// Whether the gateway under test serves the decoder: said by whoever configured
/// it, because asking the gateway would let one that lost its decoder pass as one
/// configured without.
const SERVES_DECODER = process.env.REMOTEX_PLAYWRIGHT_HEVC_WASM !== "0";

/// The wire, copied from src/protocol.rs rather than imported from the SPA.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_VIDEO = 0x03;
const VIDEO_KEYFRAME = 0x01;

interface Session {
  /** The session socket's `apple_media`: the browser's own decoder's answer. */
  appleMedia?: string;
  /** The `choices` of every `connect` the page sent. */
  connects: { passthrough?: boolean; software?: boolean }[];
  formats: { decode: string; passthrough: boolean; software: unknown }[];
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
  const seen: Session = {
    connects: [],
    formats: [],
    acks: [],
    refreshes: [],
    decoderFiles: [],
  };
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
      ws.on("framesent", ({ payload }) => {
        if (typeof payload === "string") {
          const message = JSON.parse(payload);
          if (message.type === "connect") {
            seen.connects.push(message.choices);
          }
        }
      });
      return;
    }
    if (url.pathname !== "/ws/display") {
      return;
    }
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload === "string") {
        const message = JSON.parse(payload);
        if (message.type === "videoFormat") {
          seen.formats.push({
            decode: message.decode,
            passthrough: message.passthrough === true,
            software: message.software,
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

/// What the gateway lists the target as offering of the page's decoders, asked of
/// it directly with the page's login.
async function offered(page: Page): Promise<unknown> {
  const response = await page.request.get(
    new URL("/api/targets", BASE_URL).toString(),
  );
  const targets: { name: string; software: unknown }[] = await response.json();
  return targets.find((target) => target.name === HEVC_TARGET)?.software;
}

test.describe("a High Performance target and the page's software decoder", () => {
  test.skip(
    !HEVC_TARGET,
    "set REMOTEX_PLAYWRIGHT_HEVC_TARGET=<target> against a gateway with an ard-high-performance target",
  );
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("with the decoder served and chosen, the passed stream is decoded in the page", async ({
    page,
  }) => {
    test.skip(!SERVES_DECODER, "the gateway has no decoder archive");
    const seen = watchSession(page);
    await logInAndConnectTo(page, HEVC_TARGET ?? "", "", {
      passthrough: true,
      software: true,
    });

    expect(await offered(page)).toMatchObject({ hevc: true });
    expect(await page.evaluate(() => globalThis.crossOriginIsolated)).toBe(true);
    expect(seen.connects.at(-1)).toMatchObject({
      passthrough: true,
      software: true,
    });
    await expect
      .poll(() => seen.formats.some((f) => f.passthrough), { timeout: 20_000 })
      .toBe(true);
    for (const format of seen.formats.filter((f) => f.passthrough)) {
      expect(format.decode).toMatch(/^hev1\./);
      expect(format.software, "told to decode it in the page").toBe(true);
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
    // shows from the first one (planesPicture.ts).
    await expect(page.locator("canvas.graphics")).toBeVisible();

    // Loaded by the decode worker, and asked for by nothing before it: whether
    // the gateway serves it is the target listing's to say.
    expect(seen.decoderFiles).toContain("GET /hevc/hevc.js 200");
    expect(seen.decoderFiles).toContain("GET /hevc/hevc.wasm 200");
    expect(seen.decoderFiles.filter((file) => file.startsWith("HEAD"))).toEqual(
      [],
    );
  });

  test("without the decoder, a browser whose own refuses the HEVC takes VP9", async ({
    page,
  }) => {
    test.skip(SERVES_DECODER, "the gateway has the decoder archive");
    const seen = watchSession(page);
    // The passthrough is greyed at the picker for a page that cannot decode the
    // stream, so the session starts without it.
    await logInAndConnectTo(page, HEVC_TARGET ?? "");

    expect(await offered(page)).toMatchObject({ hevc: false });
    expect(seen.appleMedia, "this browser's own decoder").toBe("false");
    expect(seen.connects.at(-1)?.passthrough).toBe(false);
    await expect
      .poll(() => seen.formats.length, { timeout: 20_000 })
      .toBeGreaterThan(0);
    for (const format of seen.formats) {
      expect(format.passthrough).toBe(false);
      expect(format.decode).toMatch(/^vp09\./);
    }
    expect(seen.decoderFiles).toEqual([]);
  });
});
