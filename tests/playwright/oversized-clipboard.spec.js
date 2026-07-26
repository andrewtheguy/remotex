// The refusal path, end to end against the live Mac: a pasteboard larger than
// MAX_CLIPBOARD_BYTES is reported as its size rather than transferred in part.
//
// Worth a live test rather than only unit coverage, because the claim spans five
// layers that each used to truncate independently — agent, gateway, browser link,
// panel state, and the panel's own buttons — and the interesting failure is a
// truncated value arriving *successfully*, which no single layer can catch.
const { test, expect } = require("@playwright/test");
const {
  MISSING_ENV,
  logInAndConnect,
  openClipboardPanel,
  readRemoteClipboard,
  returnToPicker,
  setRemoteClipboard,
  setRemoteClipboardBytes,
} = require("./support.js");

const LIMIT = 65_536;
const OVERSIZED = 200_000;

test("a remote clipboard over the limit is reported, not truncated", async ({
  page,
}) => {
  test.setTimeout(90_000);
  test.skip(
    MISSING_ENV.length > 0,
    `live Mac clipboard test requires ${MISSING_ENV.join(", ")}`,
  );

  const pageErrors = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));

  await logInAndConnect(page);

  // Set *after* the session is up, on purpose: the agent baselines the
  // pasteboard's change count when the watch starts, so a clipboard that was
  // already there is not a change and is never read at all.
  const localSentinel = `remotex-ui-local-${Date.now()}`;
  await page.evaluate(
    (text) => navigator.clipboard.writeText(text),
    localSentinel,
  );
  setRemoteClipboardBytes(OVERSIZED);

  await openClipboardPanel(page);

  // No CRC32 line: there are no bytes here to checksum. The size and the limit
  // are the whole of what the panel knows.
  const card = page.getByRole("button", {
    name: "Remote clipboard too large; switch to typing",
  });
  await expect(card).toBeVisible({ timeout: 20_000 });
  await expect(card).toContainText(`LEN ${OVERSIZED}B`);
  await expect(card).toContainText(`LIMIT ${LIMIT}B`);
  await expect(card).toContainText("Too large to transfer");
  await expect(card).not.toContainText("CRC32");

  // The refusal costs the local clipboard nothing — the whole point of not
  // mirroring a value that was never transferred.
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe(localSentinel);

  // Copy names the size as the reason. "Nothing to copy" would be the answer for
  // a remote that copied nothing, which is the case this is kept apart from.
  await page.getByRole("button", { name: "Copy" }).click();
  await expect(
    page.getByText("Remote clipboard too large to transfer"),
  ).toBeVisible();
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe(localSentinel);

  // The card is also the way out of this state: it opens the editor empty, and
  // Send works from there. Without that, Send would stay disabled for as long as
  // the remote's clipboard is oversized.
  await card.click();
  const input = page.getByLabel("Clipboard text");
  await expect(input).toBeVisible();
  await expect(input).toHaveValue("");
  const typed = `remotex-ui-typed-${Date.now()}`;
  await input.fill(typed);
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.getByText("Clipboard sent to remote")).toBeVisible();
  await expect.poll(readRemoteClipboard).toBe(typed);

  // And a value that fits still syncs afterwards, so the refusal left no state
  // behind that suppresses the next copy.
  const afterValue = `remotex-ui-after-${Date.now()}`;
  setRemoteClipboard(afterValue);
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe(afterValue);

  await returnToPicker(page);
  expect(pageErrors).toEqual([]);
});
