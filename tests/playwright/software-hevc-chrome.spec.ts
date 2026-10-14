// The software HEVC decoder's fallback, from the installed Google Chrome,
// whose own decoder takes a High Performance Mac's HEVC where the GPU decodes
// HEVC Range Extensions 4:4:4, as on a Mac, and refuses it elsewhere. Playwright's
// bundled Chromium has no HEVC decoder at all. What is asserted, and how to run
// it, is softwareHevc.ts's.
import { test } from "@playwright/test";

import { expectDecodedAsTheBrowserSays, HEVC_TARGET } from "./softwareHevc";
import { leaveSession } from "./support";

test.use({ channel: "chrome" });

test.describe("a High Performance target, its HEVC decoded as Chrome's own decoder says", () => {
  test.skip(
    !HEVC_TARGET,
    "set REMOTEX_PLAYWRIGHT_HEVC_TARGET=<target> against a gateway with an ard-high-performance target",
  );
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("the passed stream goes to the decoder the browser's own answer calls for", async ({
    page,
  }) => {
    await expectDecodedAsTheBrowserSays(page);
  });
});
