// What a session is started with is chosen under its target at the picker, and
// this is the assertion that the choice is the one the gateway is sent, the one it
// reports back, and the one it holds a later browser to.
//
// Everything asserted is a system decision: which options the target shows, what
// `connect` carried and what `connected` answered, read from the session socket by
// this file's own parser, what the browser kept for the next visit, and what a
// browser that cannot take the session's passthrough is told and shown: an rdp
// target with its pipeline on passes it in every session, so such a browser
// cannot start it. Nothing here looks at the canvas.
//
// It needs a gateway with an `rdp` target that configures a size, the one type that
// offers all three choices. The tone harness in `src/server.rs` is one, with a
// scripted engine and no remote:
//
//     cargo test --lib serve_a_test_tone -- --ignored --nocapture
//
// then, against the address it prints:
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:PORT/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=hunter2 \
//     REMOTEX_PLAYWRIGHT_PICKER_TARGET=test-tone \
//     bunx playwright test '/picker-options\.spec\.ts$'
import { expect, type Locator, type Page, test } from "@playwright/test";

import {
  FOLLOWS_WINDOW,
  leaveSession,
  logIn,
  logInAndConnectTo,
  returnToPicker,
  targetNamePattern,
} from "./support";

/// The opt-in, and the target name in one, as the other specs have it.
const PICKER_TARGET = process.env.REMOTEX_PLAYWRIGHT_PICKER_TARGET;

interface Choices {
  size: "target" | "default" | "window";
  audio: "off" | "opus" | "flac";
  passthrough: string | null;
}

interface Session {
  /// The `choices` of every `connect` the page sent.
  connects: Choices[];
  /// Every session status the gateway sent, in order, as it came.
  statuses: Record<string, unknown>[];
}

/// Record what the session socket carried about starting a session. Registered
/// before navigation so nothing is missed.
function watchSession(page: Page): Session {
  const seen: Session = { connects: [], statuses: [] };
  page.on("websocket", (ws) => {
    if (new URL(ws.url()).pathname !== "/ws") {
      return;
    }
    ws.on("framesent", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      const message: Record<string, unknown> = JSON.parse(payload);
      if (message.type === "connect") {
        seen.connects.push(message.choices as Choices);
      }
    });
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      const message: Record<string, unknown> = JSON.parse(payload);
      if (message.type === "connected") {
        seen.statuses.push(message);
      }
    });
  });
  return seen;
}

/// Open the target at the picker, unless it is open already, and return its
/// item: the row and the options under it.
async function openTarget(page: Page): Promise<Locator> {
  const row = page.getByRole("button", {
    name: targetNamePattern(PICKER_TARGET ?? ""),
  });
  if ((await row.getAttribute("aria-expanded")) !== "true") {
    await row.click();
  }
  return page.getByRole("listitem").filter({ has: row });
}

const SOUND = /^Sound/;
const PASSED = "egfx is on for this target, so every session passes the host's graphics pipeline through";

test.describe("the picker's options", () => {
  test.skip(
    !PICKER_TARGET,
    "set REMOTEX_PLAYWRIGHT_PICKER_TARGET=<rdp target> against a gateway with one",
  );

  // Cleanup, so it runs even when an assertion above threw: see `leaveSession`.
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("Start carries what was chosen, and the session reports it back", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, PICKER_TARGET ?? "", "", {
      resize: true,
      pipeline: "adaptive-region",
    });

    // The pipeline is passed in every session of an rdp target that has it,
    // without being named, and drawn as was chosen.
    expect(seen.connects).toEqual([
      {
        size: "window",
        audio: "off",
        passthrough: null,
        placement: "right",
        pipeline: "adaptive-region",
      },
    ]);
    const connected = seen.statuses.at(-1);
    expect(connected).toMatchObject({
      type: "connected",
      name: PICKER_TARGET,
      resize: true,
      audio: false,
      passthrough: "rdp-graphics",
    });
  });

  test("a target opens to its options, and they are remembered for it", async ({
    page,
  }) => {
    await logInAndConnectTo(page, PICKER_TARGET ?? "", "", { sound: true });
    await returnToPicker(page);

    // A target starts closed, a gateway's only one included: nothing starts a
    // session until it is opened and Start is pressed.
    await expect(
      page.getByRole("button", { name: targetNamePattern(PICKER_TARGET ?? "") }),
    ).toHaveAttribute("aria-expanded", "false");
    await expect(
      page.getByRole("button", { name: "Start", exact: true }),
    ).toHaveCount(0);

    // An rdp target offers all three, and what was chosen is how it comes back.
    // The size it will have is shown before Start: the one its config sets, which
    // is the size until somebody chooses the window.
    const item = await openTarget(page);
    const sizes = item.getByRole("group", { name: "Size" }).getByRole("radio");
    await expect(sizes).toHaveCount(2);
    await expect(sizes.first()).toHaveAccessibleName(/^\d+×\d+/);
    await expect(sizes.first()).toBeChecked();
    await expect(item.getByRole("radio", { name: FOLLOWS_WINDOW })).not.toBeChecked();
    // Its sound is ticked, and under it are the two formats, the one chosen
    // shown.
    await expect(item.getByRole("checkbox", { name: SOUND })).toBeChecked();
    const formats = item
      .getByRole("radiogroup", { name: "Sound format" })
      .getByRole("radio");
    await expect(formats).toHaveCount(2);
    await expect(item.getByRole("radio", { name: /^Opus/ })).toBeChecked();
    await expect(item.getByRole("radio", { name: /^FLAC/ })).not.toBeChecked();
    // The pipeline is said to be passed, and is no choice. How the host draws
    // it is, high quality until somebody chooses adaptive, whose H.264 is then over
    // the full screen until somebody chooses the region.
    await expect(item).toContainText(PASSED);
    await expect(item.getByRole("checkbox", { name: /pass/i })).toHaveCount(0);
    const graphics = item.getByRole("group", { name: "Graphics" });
    await expect(graphics.getByRole("radio", { name: /^High quality/ })).toBeChecked();
    await expect(graphics.getByRole("radio", { name: /^H\.264/ })).toHaveCount(0);
    await graphics.getByRole("radio", { name: /^Adaptive \(alpha\)/ }).check();
    await expect(
      graphics.getByRole("radio", { name: /^H\.264 full screen/ }),
    ).toBeChecked();
    await graphics.getByRole("radio", { name: /^High quality/ }).check();
    await expect(
      item.getByRole("button", { name: "Start", exact: true }),
    ).toBeEnabled();

    // Kept by the browser, not by the page: a reload finds it as it was left.
    await item.getByRole("radio", { name: FOLLOWS_WINDOW }).check();
    await page.reload();
    await expect(
      page.getByRole("heading", { name: "Pick a target" }),
    ).toBeVisible({ timeout: 20_000 });
    const again = await openTarget(page);
    await expect(again.getByRole("radio", { name: FOLLOWS_WINDOW })).toBeChecked();
    await expect(again.getByRole("checkbox", { name: SOUND })).toBeChecked();
    await expect(again.getByRole("radio", { name: /^Opus/ })).toBeChecked();
  });

  test("a browser that takes a session over starts at the picker with its own choices", async ({
    page,
    browser,
  }) => {
    const first = watchSession(page);
    await logInAndConnectTo(page, PICKER_TARGET ?? "", "", {});
    expect(first.statuses.at(-1)).toMatchObject({
      type: "connected",
      passthrough: "rdp-graphics",
    });

    // A second browser whose page is not cross-origin isolated, which is one that
    // cannot compose a passed pipeline, takes the session over.
    const context = await browser.newContext();
    try {
      await context.addInitScript(() => {
        Object.defineProperty(globalThis, "crossOriginIsolated", {
          value: false,
        });
      });
      const other = await context.newPage();
      const second = watchSession(other);
      await logIn(other);

      // It is given neither the session nor tiles in its place: it
      // lands on the picker, where the target cannot start, with the reason.
      await expect(
        other.getByRole("heading", { name: "Pick a target" }),
      ).toBeVisible();
      expect(second.statuses).toEqual([]);
      const item = await openTarget(other);
      await expect(item).toContainText(PASSED);
      await expect(item).toContainText(
        "This browser cannot compose the host's graphics pipeline",
      );
      await expect(
        item.getByRole("button", { name: "Start", exact: true }),
      ).toBeDisabled();
    } finally {
      await context.close();
    }

    // The slot goes back to the page the cleanup knows, which the second browser
    // evicted: it lands on the picker, since the takeover ended the session.
    await page.getByRole("button", { name: "Take it back" }).click();
    await expect(
      page.getByRole("heading", { name: "Pick a target" }),
    ).toBeVisible({ timeout: 20_000 });
  });
});

/** The codec of every `audioFormat` the page's audio socket is told. */
function watchAudioFormats(page: Page): string[] {
  const formats: string[] = [];
  page.on("websocket", (ws) => {
    if (new URL(ws.url()).pathname !== "/ws/audio") {
      return;
    }
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      const message: Record<string, unknown> = JSON.parse(payload);
      if (message.type === "audioFormat") {
        formats.push(String(message.codec));
      }
    });
  });
  return formats;
}

test.describe("lossless sound", () => {
  test.skip(
    !PICKER_TARGET,
    "set REMOTEX_PLAYWRIGHT_PICKER_TARGET=<rdp target> against a gateway with one",
  );

  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("plays through the browser's own decoder", async ({ page }) => {
    const seen = watchSession(page);
    const formats = watchAudioFormats(page);
    // What the player reports when a decoder fails.
    const failures: string[] = [];
    page.on("console", (message) => {
      if (/^audio: /.test(message.text())) {
        failures.push(message.text());
      }
    });
    await logIn(page);

    const item = await openTarget(page);
    await item.getByRole("checkbox", { name: SOUND }).check();
    const flac = item.getByRole("radio", { name: /^FLAC/ });
    await expect(flac).toBeEnabled();
    await flac.check();
    await item.getByRole("button", { name: "Start", exact: true }).click();
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });
    expect(seen.connects.at(-1)).toMatchObject({ audio: "flac" });
    await expect.poll(() => formats, { timeout: 10_000 }).toEqual(["flac"]);

    // Sound arrives and decodes: the menu offers Mute, and no decoder failed.
    await page.getByRole("button", { name: "Open menu" }).click();
    await expect(page.getByRole("button", { name: "Mute" })).toBeEnabled();
    await page.getByRole("button", { name: "Close menu" }).click();
    expect(failures).toEqual([]);
    await returnToPicker(page);
  });

  test("is greyed where the browser's decoder fails a FLAC frame", async ({
    page,
  }) => {
    // Safari 27 as the page sees it: an AudioDecoder that takes the FLAC
    // configuration and then fails every frame, and plays Opus.
    await page.addInitScript(() => {
      const Native = globalThis.AudioDecoder;
      globalThis.AudioDecoder = class extends Native {
        #init: AudioDecoderInit;
        #flac = false;
        constructor(init: AudioDecoderInit) {
          super(init);
          this.#init = init;
        }
        configure(config: AudioDecoderConfig) {
          this.#flac = config.codec === "flac";
          super.configure(config);
        }
        decode(chunk: EncodedAudioChunk) {
          if (!this.#flac) {
            super.decode(chunk);
            return;
          }
          queueMicrotask(() =>
            this.#init.error(
              new DOMException(
                "InternalAudioDecoderCocoa decoding failed",
                "EncodingError",
              ),
            ),
          );
        }
      };
    });
    const seen = watchSession(page);
    await logIn(page);

    // FLAC is greyed, with the reason, and ticking the sound takes Opus.
    const item = await openTarget(page);
    await item.getByRole("checkbox", { name: SOUND }).check();
    await expect(item.getByRole("radio", { name: /^Opus/ })).toBeChecked();
    const flac = item.getByRole("radio", { name: /^FLAC/ });
    await expect(flac).toBeDisabled();
    await expect(flac).toHaveAccessibleName(/does not decode FLAC/);

    await item.getByRole("button", { name: "Start", exact: true }).click();
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });
    expect(seen.connects.at(-1)).toMatchObject({ audio: "opus" });
    await returnToPicker(page);
  });
});

test.describe("a browser with no audio decoder", () => {
  test.skip(
    !PICKER_TARGET,
    "set REMOTEX_PLAYWRIGHT_PICKER_TARGET=<rdp target> against a gateway with one",
  );

  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("starts, without sound", async ({ page }) => {
    // Safari 16.4 to 18 as the page sees it: a video decoder and no audio one.
    await page.addInitScript(() => {
      Reflect.deleteProperty(globalThis, "AudioDecoder");
    });
    const seen = watchSession(page);
    await logIn(page);

    // The sound cannot be ticked, and says why.
    const item = await openTarget(page);
    const sound = item.getByRole("checkbox", { name: SOUND });
    await expect(sound).toBeDisabled();
    await expect(sound).not.toBeChecked();
    await expect(item).toContainText("no WebCodecs AudioDecoder");

    await item.getByRole("button", { name: "Start", exact: true }).click();
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });
    expect(seen.connects.at(-1)).toMatchObject({ audio: "off" });
    expect(seen.statuses.at(-1)).toMatchObject({
      type: "connected",
      audio: false,
    });
    await returnToPicker(page);
  });
});

test.describe("the picker on a phone", () => {
  test.skip(
    !PICKER_TARGET,
    "set REMOTEX_PLAYWRIGHT_PICKER_TARGET=<rdp target> against a gateway with one",
  );

  // A phone as the page tells one: a screen whose short side is a phone's, and,
  // set in the test, the two touch points a pinch needs.
  test.use({
    viewport: { width: 390, height: 844 },
    hasTouch: true,
    isMobile: true,
  });

  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("a phone is offered sizes the desktop keeps, never its window", async ({
    page,
  }) => {
    await page.addInitScript(() => {
      Object.defineProperty(Navigator.prototype, "maxTouchPoints", {
        get: () => 5,
      });
    });
    const seen = watchSession(page);
    await logIn(page);

    // The size the target configures, and the default beside it.
    const item = await openTarget(page);
    const sizes = item.getByRole("group", { name: "Size" }).getByRole("radio");
    await expect(sizes).toHaveCount(2);
    await expect(sizes.first()).toBeChecked();
    await expect(
      item.getByRole("radio", { name: FOLLOWS_WINDOW }),
    ).toHaveCount(0);

    // Ticking the sound takes Opus.
    await item.getByRole("checkbox", { name: SOUND }).check();
    await expect(item.getByRole("radio", { name: /^Opus/ })).toBeChecked();

    await item.getByRole("radio", { name: /^1440×900/ }).check();
    await item.getByRole("button", { name: "Start", exact: true }).click();
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });
    expect(seen.connects.at(-1)).toMatchObject({
      size: "default",
      audio: "opus",
    });
    expect(seen.statuses.at(-1)).toMatchObject({
      type: "connected",
      resize: false,
    });
  });
});
