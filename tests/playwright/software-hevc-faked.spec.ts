// The software HEVC decoder's fallback, with the browser's own decoder's
// answer faked, so both sides of it run in any browser on any machine: a "no",
// which the page decodes with the gateway's decoder where it serves one, and a
// "yes", which the page keeps in a stand-in for the browser's own decoder. What is
// asserted, and how to run it, is softwareHevc.ts's.
import { test } from "@playwright/test";

import { fakeOwnDecoder } from "./ownDecoder";
import { expectDecodedAsTheBrowserSays, HEVC_TARGET } from "./softwareHevc";
import { leaveSession } from "./support";

test.describe("a High Performance target, its HEVC decoded as a faked browser's own decoder says", () => {
  test.skip(
    !HEVC_TARGET,
    "set REMOTEX_PLAYWRIGHT_HEVC_TARGET=<target> against a gateway with an ard-high-performance target",
  );
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("a browser whose own decoder takes the HEVC keeps it", async ({
    page,
  }) => {
    await fakeOwnDecoder(page, "hevc", true);
    await expectDecodedAsTheBrowserSays(page);
  });

  test("a browser whose own decoder refuses the HEVC falls back", async ({
    page,
  }) => {
    await fakeOwnDecoder(page, "hevc", false);
    await expectDecodedAsTheBrowserSays(page);
  });
});
