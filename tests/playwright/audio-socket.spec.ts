// Sound has a WebSocket of its own (`/ws/audio`), and this is the assertion that
// keeps it there.
//
// What it checks is a *system decision*, not a rendering: which socket each frame
// arrived on, and that opening and closing the socket is the whole of the
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

import { logInAndConnect, returnToPicker } from "./support";

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

/// `returnToPicker` opens the drawer itself, and the toggle is inside it — so a spec
/// that left it open would fail there looking for a button that currently says
/// "Close menu".
async function closeMenu(page: Page): Promise<void> {
  await page.getByRole("button", { name: "Close menu" }).click();
  await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible();
}

test.describe("the audio socket", () => {
  test.skip(
    !AUDIO_TARGET,
    "set REMOTEX_PLAYWRIGHT_AUDIO_TARGET=<target> against a gateway that serves audio",
  );

  test("carries sound, and the session socket carries none", async ({
    page,
  }) => {
    const traffic = watchSockets(page);
    await logInAndConnect(page);

    // Only the session socket exists until sound is asked for. Opening the audio
    // socket *is* the request — there is no message for it — so its absence here is
    // what says nothing was subscribed.
    expect(traffic.map((t) => t.url)).toEqual(["/ws"]);

    const menu = page.getByRole("button", { name: "Open menu" });
    await menu.click();
    const toggle = page.getByRole("button", { name: "Enable audio" });
    await toggle.click();
    await expect(
      page.getByRole("button", { name: "Disable audio" }),
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
      only(traffic, "/ws").binaryKinds.filter((k) => k !== BATCH_FRAME_KIND),
      "the session socket must carry batches only",
    ).toEqual([]);

    await closeMenu(page);
    await returnToPicker(page);
  });

  test("closes when audio is turned off, and leaves the session alone", async ({
    page,
  }) => {
    const traffic = watchSockets(page);
    await logInAndConnect(page);

    await page.getByRole("button", { name: "Open menu" }).click();
    await page.getByRole("button", { name: "Enable audio" }).click();
    await expect
      .poll(() => traffic.filter((t) => t.url === "/ws/audio").length, {
        timeout: 20_000,
      })
      .toBe(1);

    await page.getByRole("button", { name: "Disable audio" }).click();

    // Closing the socket is the whole of unsubscribing, so this is the assertion
    // that the toggle does anything at all.
    await expect
      .poll(
        () => traffic.filter((t) => t.url === "/ws/audio").every((t) => t.closed),
        { timeout: 20_000 },
      )
      .toBe(true);
    // And the desktop is untouched: a session must survive its sound ending.
    expect(only(traffic, "/ws").closed).toBe(false);
    await expect(page.getByRole("button", { name: "Enable audio" })).toBeVisible();

    await closeMenu(page);
    await returnToPicker(page);
  });
});
