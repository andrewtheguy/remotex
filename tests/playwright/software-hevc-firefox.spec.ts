// BETA: the software HEVC decoder's fallback, from Playwright's Firefox, whose own
// decoder refuses a High Performance Mac's HEVC. What is asserted, and how to run
// it, is softwareHevc.ts's.
import { test } from "@playwright/test";

import { expectDecodedAsTheBrowserSays, HEVC_TARGET } from "./softwareHevc";
import { leaveSession } from "./support";

// Without the config's clipboard permissions, which are Chromium's names and
// which Firefox refuses: nothing here touches the clipboard.
test.use({ browserName: "firefox", permissions: [] });

test.describe("a High Performance target, its HEVC decoded as Firefox's own decoder says", () => {
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
