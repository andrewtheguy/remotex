// BETA: the page's software VP9 decoder (frontend/src/softwareDecoder.ts), the
// bundled vp9-wasm module, which decodes the gateway's 4:4:4 stream in a session
// started with "Decode VP9 in this page" at the picker, on a gateway whose
// `[vp9_wasm]` enables it.
//
// What is asserted is what the system decides: what the gateway lists the target
// as offering, the row the picker then shows and what Start sends, the chroma the
// page states on its session socket, what the gateway announces — the stream, and
// that this page decodes it — whether the page loads the module, which is only
// ever fetched to decode with, and — for the one claim about decoding itself —
// that the first keyframe's batch was acknowledged with no decoder failure before
// it. That last holds by ordering, not timing: a failed decoder settles its unit
// and reports the failure in the same turn, and the paint worker posts the report
// before the acknowledgement (framePainter.ts, useRemoteDesktop.ts). Which canvas
// the page shows says which path presented: the module's planes are drawn on the
// one over the desktop's.
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
// Against a gateway without the table, set REMOTEX_PLAYWRIGHT_VP9_WASM=0: the
// picker must then have no such row.
import { expect, type Page, test } from "@playwright/test";

import {
  BASE_URL,
  leaveSession,
  logIn,
  logInAndConnectTo,
  returnToPicker,
  startTarget,
  targetNamePattern,
} from "./support";

/// The opt-in, and the target name in one, as the video spec's.
const VP9_TARGET = process.env.REMOTEX_PLAYWRIGHT_VP9_TARGET;

/// Whether the gateway under test enables the module: said by whoever configured
/// it, and held against what the gateway itself lists.
const ENABLED = process.env.REMOTEX_PLAYWRIGHT_VP9_WASM !== "0";

/// The module in the bundle, under the name the build gives it.
const MODULE = /^\/assets\/vp9_bg-[\w-]+\.wasm$/;

/// The picker's row, by its label.
const ROW = /^Decode (VP9|HEVC) in this page/;

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
  /** Each session socket's `chroma`, the page's answer, in the order opened. */
  chromas: (string | null)[];
  /** The `choices` of every `connect` the page sent. */
  connects: { software?: boolean }[];
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
    chromas: [],
    connects: [],
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
      seen.chromas.push(url.searchParams.get("chroma"));
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

/// What the gateway lists the target as offering of the page's decoders, asked of
/// it directly with the page's login.
async function offered(page: Page): Promise<unknown> {
  const response = await page.request.get(
    new URL("/api/targets", BASE_URL).toString(),
  );
  const targets: { name: string; software: unknown }[] = await response.json();
  return targets.find((target) => target.name === VP9_TARGET)?.software;
}

/// Every format the newest display socket announced, once it has announced one.
async function announced(seen: Session, sockets: number): Promise<Format[]> {
  await expect
    .poll(() => seen.sockets.length >= sockets && seen.sockets.at(-1)?.length, {
      timeout: 20_000,
    })
    .toBeTruthy();
  return seen.sockets.at(-1) ?? [];
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

  test("chosen at the picker, 4:4:4 is decoded in the module, and again by the page that comes back", async ({
    page,
  }) => {
    test.skip(!ENABLED, "the gateway does not set [vp9_wasm]");
    const seen = watchSession(page);
    await logInAndConnectTo(page, VP9_TARGET ?? "", "", { software: true });

    expect(await offered(page)).toMatchObject({ vp9: true });
    expect(await page.evaluate(() => globalThis.crossOriginIsolated)).toBe(true);
    expect(seen.connects.at(-1)?.software, "Start sent the choice").toBe(true);
    for (const format of await announced(seen, 1)) {
      expect(format.decode, "profile 1, which the module decodes").toMatch(
        /^vp09\.01\./,
      );
      expect(format.software, "told to decode it in the page").toBe(true);
    }
    await firstKeyframeAcknowledged(page, seen);
    expect(seen.refreshes, "repaints the page asked for").toEqual([]);
    // Its pictures are drawn on the canvas over the desktop's, which the page
    // shows from the first one (planesPicture.ts).
    await expect(page.locator("canvas.graphics")).toBeVisible();
    // Loaded by the decode worker and compiled once: its threads are given the
    // compiled module.
    expect(seen.moduleLoads).toEqual([200]);

    // The session's, not this page's: a page that comes back to it made no choice
    // and is told the same of the stream it is repainted with.
    await page.reload();
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });
    expect(seen.connects, "the page that came back started nothing").toHaveLength(1);
    const again = await announced(seen, 2);
    for (const format of again) {
      expect(format).toMatchObject({ software: true });
      expect(format.decode).toMatch(/^vp09\.01\./);
    }
    await firstKeyframeAcknowledged(page, seen);
    await expect(page.locator("canvas.graphics")).toBeVisible();
    expect(seen.moduleLoads).toHaveLength(2);
  });

  test("a browser whose own decoder refuses profile 1 finds the row ticked, and is sent 4:4:4 all the same", async ({
    page,
  }) => {
    test.skip(!ENABLED, "the gateway does not set [vp9_wasm]");
    // Such a browser, as far as the page's one question of it goes.
    await page.addInitScript(() => {
      const ask = VideoDecoder.isConfigSupported.bind(VideoDecoder);
      VideoDecoder.isConfigSupported = (config) =>
        config.codec.startsWith("vp09.01.")
          ? Promise.resolve({ supported: false, config })
          : ask(config);
    });
    const seen = watchSession(page);
    // The row is left as the picker shows it.
    await logInAndConnectTo(page, VP9_TARGET ?? "");

    expect(seen.chromas.at(-1), "its own decoder's answer").toBe("420");
    expect(seen.connects.at(-1)?.software).toBe(true);
    for (const format of await announced(seen, 1)) {
      expect(format.decode).toMatch(/^vp09\.01\./);
      expect(format.software).toBe(true);
    }
    await firstKeyframeAcknowledged(page, seen);
    await expect(page.locator("canvas.graphics")).toBeVisible();
    expect(seen.moduleLoads).toEqual([200]);
  });

  test("not chosen, the browser's own decoder decodes what it asked for", async ({
    page,
  }) => {
    test.skip(!ENABLED, "the gateway does not set [vp9_wasm]");
    const seen = watchSession(page);
    await logInAndConnectTo(page, VP9_TARGET ?? "", "", { software: false });

    // Chromium decodes profile 1 itself.
    expect(seen.chromas.at(-1)).toBe("444");
    expect(seen.connects.at(-1)?.software).toBe(false);
    for (const format of await announced(seen, 1)) {
      expect(format.software).toBe(false);
    }
    await firstKeyframeAcknowledged(page, seen);
    await expect(page.locator("canvas.graphics")).toBeHidden();
    expect(seen.moduleLoads).toEqual([]);
  });

  test("the choice is each session's: unticked for the next one, the browser decodes that one", async ({
    page,
  }) => {
    test.skip(!ENABLED, "the gateway does not set [vp9_wasm]");
    const seen = watchSession(page);
    await logInAndConnectTo(page, VP9_TARGET ?? "", "", { software: true });
    for (const format of await announced(seen, 1)) {
      expect(format.software).toBe(true);
    }
    await firstKeyframeAcknowledged(page, seen);
    await expect(page.locator("canvas.graphics")).toBeVisible();
    await expectInfo(page, "decoded by this page's WebAssembly decoder");

    await returnToPicker(page);
    const formats = () => seen.sockets.flat();
    const before = formats().length;
    await startTarget(page, VP9_TARGET ?? "", { software: false });
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });
    expect(seen.connects.map((connect) => connect.software)).toEqual([
      true,
      false,
    ]);
    await expect
      .poll(() => formats().length, { timeout: 20_000 })
      .toBeGreaterThan(before);
    for (const format of formats().slice(before)) {
      expect(format.software, "the second session's format").toBe(false);
    }
    await expect(page.locator("canvas.graphics")).toBeHidden();
    await expectInfo(page, "decoded by the browser's native decoder");
  });

  test("on a gateway that does not enable it, the picker has no such row", async ({
    page,
  }) => {
    test.skip(ENABLED, "the gateway sets [vp9_wasm]");
    await logIn(page);
    if (await page.getByRole("button", { name: "Open menu" }).isVisible()) {
      await returnToPicker(page);
    }
    expect(await offered(page)).toMatchObject({ vp9: false });
    const row = page.getByRole("button", {
      name: targetNamePattern(VP9_TARGET ?? ""),
    });
    await row.click();
    const item = page.getByRole("listitem").filter({ has: row });
    await expect(item.getByRole("button", { name: "Start", exact: true })).toBeVisible();
    await expect(item.getByRole("checkbox", { name: ROW })).toHaveCount(0);
  });
});
