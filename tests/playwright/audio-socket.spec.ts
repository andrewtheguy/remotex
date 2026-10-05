// Sound has a WebSocket of its own (`/ws/audio`), and this is the assertion that
// keeps it there.
//
// What it checks is a *system decision*, not a rendering: which socket each frame
// arrived on, that a session started with sound opens the socket and one started
// without it does not, and that opening and closing it is the whole of the
// subscription. Nothing here looks at the canvas, counts packets against a clock, or
// asserts that anything was audible — the browser cannot tell a quiet remote from a
// broken one, and neither can a test.
//
// It needs a gateway with a target that actually produces audio, which the tone
// harness in `src/server.rs` provides deterministically and without a remote:
//
//     cargo test --lib serve_a_test_tone -- --ignored --nocapture
//
// then, against the address it prints:
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:PORT/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=hunter2 \
//     REMOTEX_PLAYWRIGHT_AUDIO_TARGET=test-tone \
//     npx playwright test audio-socket
import { expect, type Page, test } from "@playwright/test";

import { leaveSession, logInAndConnectTo } from "./support";

/// The binary frame kinds, copied rather than imported: this spec is the independent
/// check that the gateway put audio where it said it did, and reading the SPA's own
/// parser to decide that would be asking the accused.
const BATCH_FRAME_KIND = 0x02;
const AUDIO_FRAME_KIND = 0x03;

/// The opt-in, and the target name in one. Its presence is the claim that this
/// gateway has a target which produces sound; without one the spec would be asserting
/// against silence and would pass for the wrong reason.
const AUDIO_TARGET = process.env.REMOTEX_PLAYWRIGHT_AUDIO_TARGET;

interface Traffic {
  url: string;
  binaryKinds: number[];
  controlTypes: string[];
  closed: boolean;
}

/// Record every socket the page opens, by URL, with the kinds and control types that
/// arrived on each. Registered before navigation so nothing is missed.
function watchSockets(page: Page): Traffic[] {
  const seen: Traffic[] = [];
  page.on("websocket", (ws) => {
    const traffic: Traffic = {
      url: new URL(ws.url()).pathname,
      binaryKinds: [],
      controlTypes: [],
      closed: false,
    };
    seen.push(traffic);
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload === "string") {
        const type: unknown = JSON.parse(payload).type;
        if (typeof type === "string") {
          traffic.controlTypes.push(type);
        }
        return;
      }
      traffic.binaryKinds.push(payload.readUInt8(0));
    });
    ws.on("close", () => {
      traffic.closed = true;
    });
  });
  return seen;
}

const only = (traffic: Traffic[], path: string): Traffic => {
  const matches = traffic.filter((t) => t.url === path && !t.closed);
  expect(matches, `open sockets at ${path}`).toHaveLength(1);
  return matches[0];
};

test.describe("the audio socket", () => {
  test.skip(
    !AUDIO_TARGET,
    "set REMOTEX_PLAYWRIGHT_AUDIO_TARGET=<target> against a gateway that serves audio",
  );

  // Cleanup, so it runs even when an assertion above threw: see `leaveSession`.
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  const start = (page: Page, sound: boolean) =>
    logInAndConnectTo(page, AUDIO_TARGET ?? "", "", { sound });

  // Whether a session carries the remote's sound is chosen before it starts. One
  // started without it asks the remote for none, so there is no socket for it and
  // nothing in the menu to unmute.
  test("is not opened by a session started without sound", async ({ page }) => {
    const traffic = watchSockets(page);
    await start(page, false);

    await page.getByRole("button", { name: "Open menu" }).click();
    await expect(page.getByRole("button", { name: "End session" })).toBeVisible();
    await expect(page.getByRole("button", { name: /^(Mute|Unmute)$/ })).toHaveCount(0);
    expect(traffic.map((t) => t.url).sort()).toEqual(["/ws", "/ws/display"]);
  });

  test("carries sound, and the session socket carries none", async ({
    page,
  }) => {
    const traffic = watchSockets(page);
    await start(page, true);

    // A session started with sound comes up unmuted: Start's click is the gesture,
    // and opening the audio socket *is* the subscription — there is no message for
    // it.
    await page.getByRole("button", { name: "Open menu" }).click();
    await expect(
      page.getByRole("button", { name: "Mute", exact: true }),
    ).toHaveAttribute("aria-pressed", "true");

    // The format is what configures a decoder, and it must arrive on the socket that
    // will carry the packets — not on the one that carries pixels.
    await expect
      .poll(() => only(traffic, "/ws/audio").controlTypes, { timeout: 20_000 })
      .toContain("audioFormat");
    expect(
      only(traffic, "/ws").controlTypes,
      "the session socket must not carry the audio format",
    ).not.toContain("audioFormat");

    // And then the sound itself. Polled rather than awaited on a deadline: the
    // harness alternates playing and quiet phases, so *when* the first packet lands
    // is the remote's business. That it lands here and nowhere else is not.
    await expect
      .poll(() => only(traffic, "/ws/audio").binaryKinds.length, {
        timeout: 20_000,
      })
      .toBeGreaterThan(0);
    expect(
      new Set(only(traffic, "/ws/audio").binaryKinds),
      "the audio socket carries audio frames and nothing else",
    ).toEqual(new Set([AUDIO_FRAME_KIND]));
    expect(
      only(traffic, "/ws").binaryKinds,
      "the session socket carries no binary frames",
    ).toEqual([]);
    expect(
      only(traffic, "/ws/display").binaryKinds.filter(
        (k) => k !== BATCH_FRAME_KIND,
      ),
      "the display socket must carry batches only",
    ).toEqual([]);
  });

  test("closes on Mute, stays muted across a reload, and leaves the session alone", async ({
    page,
  }) => {
    const traffic = watchSockets(page);
    await start(page, true);
    await expect
      .poll(() => traffic.filter((t) => t.url === "/ws/audio").length, {
        timeout: 20_000,
      })
      .toBe(1);

    await page.getByRole("button", { name: "Open menu" }).click();
    await page.getByRole("button", { name: "Mute", exact: true }).click();

    // Closing the socket is the whole of unsubscribing, so this is the assertion
    // that the button does anything at all.
    await expect
      .poll(
        () => traffic.filter((t) => t.url === "/ws/audio").every((t) => t.closed),
        { timeout: 20_000 },
      )
      .toBe(true);
    // And the desktop is untouched: a session must survive its sound ending.
    expect(only(traffic, "/ws").closed).toBe(false);
    expect(only(traffic, "/ws/display").closed).toBe(false);
    await expect(
      page.getByRole("button", { name: "Unmute", exact: true }),
    ).toBeVisible();

    // The mute is this tab's, for this session: a reload reattaches to the session
    // and must not start playing what was muted.
    await page.reload();
    await page
      .getByRole("button", { name: "Open menu" })
      .click({ timeout: 20_000 });
    await expect(
      page.getByRole("button", { name: "Unmute", exact: true }),
    ).toBeVisible();
  });

  // Headless Chromium is not WebKit, so a reload — which reattaches this tab to its
  // session with no click at all — must ask for the sound again by itself rather
  // than come back silent.
  test("stays on across a reload", async ({ page }) => {
    const traffic = watchSockets(page);
    await start(page, true);
    await expect
      .poll(() => traffic.filter((t) => t.url === "/ws/audio").length, {
        timeout: 20_000,
      })
      .toBe(1);

    await page.reload();
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });

    // The reattach opens a second audio socket, and the format arriving on it is
    // the gateway's answer to that subscription. The second one by position: the
    // page that opened the first is gone, and its socket is not always reported
    // closed.
    await expect
      .poll(
        () =>
          traffic.filter((t) => t.url === "/ws/audio")[1]?.controlTypes ?? [],
        { timeout: 20_000 },
      )
      .toContain("audioFormat");
    expect(traffic.filter((t) => t.url === "/ws/audio")).toHaveLength(2);
    await page.getByRole("button", { name: "Open menu" }).click();
    await expect(
      page.getByRole("button", { name: "Mute", exact: true }),
    ).toHaveAttribute("aria-pressed", "true");
  });

  test.describe("on a touch client", () => {
    // A touch client as the page tells one: the two touch points a pinch needs.
    test.use({ hasTouch: true });

    test("comes up muted, and Unmute is what opens it", async ({ page }) => {
      await page.addInitScript(() => {
        Object.defineProperty(Navigator.prototype, "maxTouchPoints", {
          get: () => 5,
        });
      });
      const traffic = watchSockets(page);
      await start(page, true);

      // The session carries sound, so the button is there; nothing has subscribed.
      await page.getByRole("button", { name: "Open menu" }).click();
      const unmute = page.getByRole("button", { name: "Unmute", exact: true });
      await expect(unmute).toBeVisible();
      expect(traffic.filter((t) => t.url === "/ws/audio")).toHaveLength(0);

      await unmute.click();
      await expect
        .poll(() => only(traffic, "/ws/audio").controlTypes, {
          timeout: 20_000,
        })
        .toContain("audioFormat");
    });
  });
});
