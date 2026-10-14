// The software HEVC decoder (frontend/src/softwareDecoder.ts), which a
// gateway that has its release archive serves at /hevc/ and which decodes a High
// Performance Mac's passed stream in a page whose browser's own decoder refuses
// it. No picker row chooses it: each page falls back to it by itself.
//
// Whether a browser's own decoder takes the Mac's HEVC is the machine's to say,
// not the spec's: Chrome takes it on a Mac, and refuses it on a PC whose GPU does
// not decode HEVC Range Extensions 4:4:4. So the test asks the browser itself, apart
// from the page, and asserts the decision that answer calls for
// (`expectDecodedAsTheBrowserSays`): the browser's own decoder where it says yes,
// and otherwise the page's where the gateway serves it, and where it does not the
// Mac's H.264, which no browser is asked about.
// It runs in two browsers, a file each, so one machine can see both sides:
// software-hevc-chrome.spec.ts, the installed Google Chrome (`channel: "chrome"`;
// Playwright's bundled Chromium has no HEVC decoder at all), and
// software-hevc-firefox.spec.ts, Playwright's Firefox, whose own decoder refuses
// the HEVC. Each test notes in the report which decoder it saw (`decoder`).
//
// software-hevc-faked.spec.ts runs the same test in any browser with the answer
// faked (`fakeOwnDecoder`, ownDecoder.ts), so both sides run on every machine: a "no", after
// which everything is real, and a "yes", with a stand-in for the browser's
// decoder where the page would build it, so a machine without HEVC passes too.
//
// What is asserted is what the system decides: what the gateway lists the target
// as offering, what the session socket states, what Start sends, what the gateway
// then announces — the passed stream — which files the page loads, and — for
// the one claim about decoding itself — that the first passed keyframe's batch was
// acknowledged with no decoder failure before it. That last holds by ordering, not
// timing: a failed decoder settles its unit and reports the failure in the same
// turn, and the paint worker posts the report before the acknowledgement, so the
// page has its banner up and has sent any repaint request before the gateway reads
// the ack (framePainter.ts, useRemoteDesktop.ts).
//
// It needs Google Chrome installed, Playwright's Firefox (`bunx playwright install
// firefox`), and a gateway whose local config has an `ard-high-performance`
// target, which only a live Mac serves, and the release archive beside the config
// or named by `[hevc_wasm]`:
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
// same target must then offer no such decoder.
//
// This module is what the two share: the test, the session it watches, and what
// it reads of it.
import { expect, type Page, test } from "@playwright/test";

import { BASE_URL, logInAndConnectTo } from "./support";

/// The opt-in, and the target name in one, as the video spec's.
export const HEVC_TARGET = process.env.REMOTEX_PLAYWRIGHT_HEVC_TARGET;

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
  /** The session socket's `page_hevc`: whether the page runs the decoder. */
  pageHevc?: string;
  /** The `choices` of every `connect` the page sent. */
  connects: Record<string, unknown>[];
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
      seen.pageHevc = url.searchParams.get("page_hevc") ?? undefined;
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

/// That the first passed keyframe's batch was acknowledged with no video error
/// or repaint request before it: decoded, or failed and said so first.
async function expectPassedPicture(page: Page, seen: Session): Promise<void> {
  await expect
    .poll(() => seen.formats.some((f) => f.passthrough), { timeout: 20_000 })
    .toBe(true);
  for (const format of seen.formats.filter((f) => f.passthrough)) {
    expect(format.decode).toMatch(/^hev1\./);
  }
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
}

/// The Mac's HEVC as the page asks its browser about it: macwork's stream,
/// Range Extensions, level 5.0, the 4:4:4 constraint flags (APPLE_HEVC_PROBE in
/// frontend/src/appleMedia.ts), copied rather than imported.
const MAC_HEVC = "hev1.4.10.L150.BE.8";

/// Whether this browser's own decoder takes the Mac's HEVC, asked of it here and
/// not read from the page: a definite yes, as the page counts one.
async function ownDecoderTakes(page: Page): Promise<boolean> {
  await page.goto(BASE_URL);
  return page.evaluate(async (codec) => {
    try {
      const support = await VideoDecoder.isConfigSupported({ codec });
      return support.supported === true;
    } catch {
      return false;
    }
  }, MAC_HEVC);
}

/// Start the target with its HEVC passed where this browser can take it, and
/// assert the decoder its own answer calls for: its own where it takes the HEVC,
/// otherwise the page's where the gateway serves it, and otherwise none, the
/// HEVC greyed and the session passed the Mac's H.264.
export async function expectDecodedAsTheBrowserSays(page: Page): Promise<void> {
  const seen = watchSession(page);
  const native = await ownDecoderTakes(page);
  const passed = native || SERVES_DECODER;
  test.info().annotations.push({
    type: "decoder",
    description: native
      ? "the browser's own"
      : SERVES_DECODER
        ? "the page's"
        : "none: the session takes the Mac's H.264",
  });
  await logInAndConnectTo(page, HEVC_TARGET ?? "", "", {
    passthrough: passed ? "apple-media" : "apple-h264",
  });

  expect(await offered(page)).toBe(SERVES_DECODER);
  expect(await page.evaluate(() => globalThis.crossOriginIsolated)).toBe(true);
  expect(seen.appleMedia, "this browser's own decoder").toBe(String(native));
  // Stated whatever the browser's own answer: keeping the stream in that decoder
  // is the page's choice and not its only way.
  expect(seen.pageHevc, "this page's").toBe("true");
  expect(seen.connects.at(-1)?.passthrough).toBe(
    passed ? "apple-media" : "apple-h264",
  );

  if (!passed) {
    await expect
      .poll(() => seen.formats.length, { timeout: 20_000 })
      .toBeGreaterThan(0);
    for (const format of seen.formats) {
      expect(format.passthrough).toBe(true);
      expect(format.decode).toMatch(/^avc1\./);
    }
    expect(seen.decoderFiles).toEqual([]);
    return;
  }

  await expectPassedPicture(page, seen);
  if (native) {
    expect(seen.decoderFiles, "the page's decoder, not loaded").toEqual([]);
    return;
  }
  // Its pictures are drawn on the canvas over the desktop's, which the page
  // shows from the first one (planesPicture.ts).
  await expect(page.locator("canvas.graphics")).toBeVisible();
  // Loaded by the decode worker, and asked for by nothing before it: whether
  // the gateway serves it is the target listing's to say. The glue, hevc.js, is
  // not asserted: Playwright's Firefox does not report the worker's dynamic
  // import of it. The module is fetched by that glue (softwareDecoderModule.ts),
  // so its request is the glue's having run.
  expect(seen.decoderFiles).toContain("GET /hevc/hevc.wasm 200");
  expect(seen.decoderFiles.filter((file) => file.startsWith("HEAD"))).toEqual(
    [],
  );
}
