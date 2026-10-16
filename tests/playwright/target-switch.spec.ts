// Switching between a Mac's and an RDP host's targets on one page, without a
// reload, and what each session's end lets go of (frontend/src/desktopPainter.ts):
// a session that loaded a module of the paint worker's own, the pipeline's
// compositor or the Adaptive picture's decoder, ends that worker and the next
// session paints in another; any other session keeps it, and ends its VP9 and HEVC
// decode workers.
//
// Everything asserted is decided by the system and not by a machine's timing: that
// each session's picture starts as its target passes or encodes it and is
// acknowledged, which paint worker it was painted by, as Playwright reports them
// starting and closing, and how often each decoder's module is fetched, which is
// once for each decode worker that loads it. The browser's own decoder is made to
// refuse the gateway's VP9 and the Mac's HEVC (`fakeOwnDecoder`), so both are the
// page's to decode in its modules. Nothing here looks at a pixel.
//
// It needs a gateway whose local config names a live RDP host twice, once with the
// pipeline on and once with `egfx = false`, and a live Mac as an `ard` and an
// `ard-high-performance` target, with the HEVC decoder's release archive. Keep
// that gitignored file under `tmp/`, for example `tmp/qa_switch.toml`:
//
//     cargo run -- serve --config tmp/qa_switch.toml
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52893/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_SWITCH_RDP_TARGET=windows-arm \
//     REMOTEX_PLAYWRIGHT_SWITCH_RDP_BITMAP_TARGET=windows-bitmap \
//     REMOTEX_PLAYWRIGHT_SWITCH_MAC_TARGET=macvmstd \
//     REMOTEX_PLAYWRIGHT_SWITCH_HEVC_TARGET=macvmhighperf \
//     bunx playwright test '/target-switch\.spec\.ts$'
import { expect, type Page, test, type Worker } from "@playwright/test";

import { fakeOwnDecoder } from "./ownDecoder";
import {
  leaveSession,
  logInAndConnectTo,
  returnToPicker,
  type StartChoices,
  startTarget,
  targetNamePattern,
} from "./support";

const RDP_TARGET = process.env.REMOTEX_PLAYWRIGHT_SWITCH_RDP_TARGET;
const RDP_BITMAP_TARGET =
  process.env.REMOTEX_PLAYWRIGHT_SWITCH_RDP_BITMAP_TARGET;
const MAC_TARGET = process.env.REMOTEX_PLAYWRIGHT_SWITCH_MAC_TARGET;
const HEVC_TARGET = process.env.REMOTEX_PLAYWRIGHT_SWITCH_HEVC_TARGET;

/// The modules' files, as the bundle and the gateway name them.
const VP9_MODULE = /^\/assets\/vp9_bg-[\w-]+\.wasm$/;
const HEVC_MODULE = /^\/hevc\/[\w-]+\.wasm$/;

/// What the page was told and did, in the order it happened, over every socket.
type Event = { control: string; decode?: string } | { ack: number };

interface Watched {
  events: Event[];
  /// The paint workers, in the order they started.
  painters: Worker[];
  closed: Set<Worker>;
  vp9Loads: number;
  hevcLoads: number;
}

/// Watch both sockets, the paint workers and the modules' files. Registered
/// before navigation.
function watch(page: Page): Watched {
  const seen: Watched = {
    events: [],
    painters: [],
    closed: new Set(),
    vp9Loads: 0,
    hevcLoads: 0,
  };
  page.on("worker", (worker) => {
    if (/desktopPainter\.worker/.test(worker.url())) {
      seen.painters.push(worker);
      worker.once("close", () => seen.closed.add(worker));
    }
  });
  // The context's, not the page's: a module is fetched by a decode worker.
  page.context().on("response", (response) => {
    const path = new URL(response.url()).pathname;
    if (VP9_MODULE.test(path)) {
      seen.vp9Loads += 1;
    } else if (HEVC_MODULE.test(path)) {
      seen.hevcLoads += 1;
    }
  });
  page.on("websocket", (ws) => {
    const path = new URL(ws.url()).pathname;
    if (path !== "/ws" && path !== "/ws/display") {
      return;
    }
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      const message = JSON.parse(payload);
      if (typeof message.type === "string") {
        seen.events.push({ control: message.type, decode: message.decode });
      }
    });
    ws.on("framesent", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      const message = JSON.parse(payload);
      if (message.type === "paintAck") {
        seen.events.push({ ack: message.sequence });
      }
    });
  });
  return seen;
}

/// How a session's picture starts: the control message that announces it.
type Start =
  | { control: "graphicsStart" | "adaptiveStart" }
  | { control: "videoFormat"; decode: RegExp };

const PIPELINE: Start = { control: "graphicsStart" };
const ADAPTIVE: Start = { control: "adaptiveStart" };
const VP9: Start = { control: "videoFormat", decode: /^vp09\.01\./ };
const HEVC: Start = { control: "videoFormat", decode: /^hev1\./ };

/// That the session started at `from` announced its picture as `start`, and the
/// page acknowledged a batch after that.
async function expectPainted(
  seen: Watched,
  from: number,
  start: Start,
): Promise<void> {
  await expect
    .poll(
      () => {
        const events = seen.events.slice(from);
        const at = events.findIndex(
          (event) =>
            "control" in event &&
            event.control === start.control &&
            (start.control !== "videoFormat" ||
              start.decode.test(event.decode ?? "")),
        );
        return at >= 0 && events.slice(at).some((event) => "ack" in event);
      },
      { message: `a ${start.control} and a batch painted after it`, timeout: 30_000 },
    )
    .toBe(true);
}

/// Start `target` from the picker, and wait for its picture. A Mac's picture is
/// one the picker remembers, so the gateway's VP9 is chosen by name where the
/// target offers a choice of picture, rather than left to the last session's.
async function session(
  page: Page,
  seen: Watched,
  target: string,
  start: Start,
  choices: StartChoices = {},
): Promise<void> {
  const from = seen.events.length;
  if (start === VP9) {
    const row = page.getByRole("button", { name: targetNamePattern(target) });
    if ((await row.getAttribute("aria-expanded")) !== "true") {
      await row.click();
    }
    const picture = page
      .getByRole("listitem")
      .filter({ has: row })
      .getByRole("group", { name: "Picture" });
    if ((await picture.count()) > 0) {
      await picture.getByRole("radio", { name: /^VP9/ }).check();
    }
  }
  await startTarget(page, target, choices);
  await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
    timeout: 30_000,
  });
  await expectPainted(seen, from, start);
  await expect(page.getByRole("alert")).toHaveCount(0);
}


test.describe("a Mac's and an RDP host's targets on one page", () => {
  test.skip(
    !RDP_TARGET || !RDP_BITMAP_TARGET || !MAC_TARGET || !HEVC_TARGET,
    "set the REMOTEX_PLAYWRIGHT_SWITCH_* targets against a gateway with a live RDP host and a live Mac",
  );

  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("let go of what each session's picture alone loaded as they switch", async ({
    page,
  }) => {
    test.setTimeout(300_000);
    await fakeOwnDecoder(page, "vp9", false);
    await fakeOwnDecoder(page, "hevc", false);
    const seen = watch(page);
    const rdp = RDP_TARGET ?? "";
    const bitmap = RDP_BITMAP_TARGET ?? "";
    const mac = MAC_TARGET ?? "";
    const hevc = HEVC_TARGET ?? "";
    const open = (worker: Worker | undefined) =>
      worker !== undefined && !seen.closed.has(worker);

    // The pipeline, composed in the first paint worker.
    await logInAndConnectTo(page, rdp);
    await expectPainted(seen, 0, PIPELINE);
    expect(seen.painters).toHaveLength(1);

    // Its end ends that worker; the Mac's VP9 is decoded in the next, which loads
    // the VP9's module.
    await returnToPicker(page);
    await expect.poll(() => open(seen.painters[0])).toBe(false);
    await session(page, seen, mac, VP9);
    expect(seen.painters).toHaveLength(2);
    expect(seen.vp9Loads, "the VP9's module, loaded once").toBe(1);

    // A VP9 session's end keeps the worker and ends its VP9 decode worker: the
    // RDP host's VP9 is decoded in the same paint worker, by a decode worker that
    // loads the module again.
    await returnToPicker(page);
    await session(page, seen, bitmap, VP9);
    expect(seen.painters).toHaveLength(2);
    expect(open(seen.painters[1])).toBe(true);
    expect(seen.vp9Loads, "the VP9's module, loaded again").toBe(2);

    // The Adaptive picture is decoded in that same worker, whose end it then is.
    await returnToPicker(page);
    await session(page, seen, mac, ADAPTIVE, {
      passthrough: "apple-adaptive",
    });
    expect(seen.painters).toHaveLength(2);
    await returnToPicker(page);
    await expect.poll(() => open(seen.painters[1])).toBe(false);

    // The Mac's HEVC, decoded in a third paint worker's HEVC decode worker.
    await session(page, seen, hevc, HEVC, { passthrough: "apple-media" });
    expect(seen.painters).toHaveLength(3);
    const hevcLoads = seen.hevcLoads;
    expect(hevcLoads, "the HEVC's module, loaded").toBeGreaterThan(0);

    // Its end keeps the paint worker and ends the HEVC's decode worker, so the
    // next HEVC session loads the module again in the same paint worker.
    await returnToPicker(page);
    await session(page, seen, hevc, HEVC, { passthrough: "apple-media" });
    expect(seen.painters).toHaveLength(3);
    expect(open(seen.painters[2])).toBe(true);
    expect(seen.hevcLoads, "the HEVC's module, loaded again").toBe(
      2 * hevcLoads,
    );

    // And the Mac's VP9 there loads the VP9's module once more.
    await returnToPicker(page);
    await session(page, seen, mac, VP9);
    expect(seen.painters).toHaveLength(3);
    expect(seen.vp9Loads, "the VP9's module, in the third paint worker").toBe(
      3,
    );

    // And the pipeline after it, composed in the same worker, whose end it is.
    await returnToPicker(page);
    await session(page, seen, rdp, PIPELINE);
    expect(seen.painters).toHaveLength(3);
    await returnToPicker(page);
    await expect.poll(() => open(seen.painters[2])).toBe(false);
  });
});
