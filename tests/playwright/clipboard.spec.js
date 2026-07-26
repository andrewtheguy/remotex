const { test, expect } = require("@playwright/test");
const {
  MISSING_ENV,
  logInAndConnect,
  openClipboardPanel,
  readRemoteClipboard,
  setRemoteClipboard,
} = require("./support.js");

const CRC32_POLYNOMIAL = 0xedb88320;
const CRC32_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let i = 0; i < 256; i += 1) {
    let value = i;
    for (let bit = 0; bit < 8; bit += 1) {
      value =
        (value & 1) === 1
          ? CRC32_POLYNOMIAL ^ (value >>> 1)
          : value >>> 1;
    }
    table[i] = value >>> 0;
  }
  return table;
})();

function crc32(text) {
  let crc = 0xffffffff;
  for (const byte of Buffer.from(text, "utf8")) {
    crc = (crc >>> 8) ^ CRC32_TABLE[(crc ^ byte) & 0xff];
  }
  return ((crc ^ 0xffffffff) >>> 0).toString(16).padStart(8, "0");
}

test("clipboard panel reads require explicit Copy while pushes still auto-sync", async ({
  page,
}) => {
  test.setTimeout(90_000);
  test.skip(
    MISSING_ENV.length > 0,
    `live Mac clipboard test requires ${MISSING_ENV.join(", ")}`,
  );

  const pageErrors = [];
  let remotePushes = 0;
  page.on("pageerror", (error) => pageErrors.push(error.message));
  page.on("websocket", (socket) => {
    socket.on("framereceived", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      try {
        const message = JSON.parse(payload);
        if (message.type === "clipboard" && !message.requested) {
          remotePushes += 1;
        }
      } catch {
        // Binary tile frames and unrelated non-JSON data are not clipboard
        // control messages.
      }
    });
  });

  await logInAndConnect(page);

  // Unsolicited remote changes retain the established automatic-sync path.
  const remoteValue = `remotex-ui-remote-${Date.now()}`;
  setRemoteClipboard(remoteValue);
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe(remoteValue);
  await expect.poll(() => remotePushes).toBeGreaterThan(0);

  // A repeated remote announcement can follow a guest paste even though the
  // guest clipboard content did not change. It is activity for metadata, but
  // must not replace a newer local clipboard value.
  const localSentinel = `remotex-ui-local-${Date.now()}`;
  await page.evaluate(
    (text) => navigator.clipboard.writeText(text),
    localSentinel,
  );
  const pushesBeforeRepeat = remotePushes;
  setRemoteClipboard(remoteValue);
  await expect.poll(() => remotePushes).toBeGreaterThan(pushesBeforeRepeat);
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe(localSentinel);

  // A panel Fetch and Reveal are reads. Neither may cross the explicit Copy
  // boundary and replace this unrelated local clipboard value.
  await openClipboardPanel(page);

  const metadata = page.getByRole("button", {
    name: "Reveal remote clipboard content",
  });
  await expect(metadata).toBeVisible({ timeout: 10_000 });
  await expect(metadata).toContainText(`CRC32 ${crc32(remoteValue)}`);
  await expect(metadata).toContainText(
    `LEN ${Buffer.byteLength(remoteValue)}B`,
  );
  await expect(metadata).toContainText(/AT (?!UNKNOWN).+/);
  await expect(page.getByText(remoteValue, { exact: true })).toHaveCount(0);
  await expect(page.getByLabel("Clipboard text")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Send" })).toBeDisabled();
  await expect(page.getByRole("button", { name: "Copy" })).toBeEnabled();
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe(localSentinel);

  await metadata.click();
  const input = page.getByLabel("Clipboard text");
  await expect(input).toBeVisible();
  await expect(input).toHaveValue(remoteValue);
  await expect(metadata).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Send" })).toBeEnabled();
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe(localSentinel);

  await page.getByRole("button", { name: "Copy" }).click();
  await expect(page.getByText("Clipboard copied")).toBeVisible();
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe(remoteValue);

  // The same revealed textarea remains the send surface.
  const webValue = `remotex-ui-web-${Date.now()}`;
  await input.fill(webValue);
  await expect(
    page.getByText(`${Buffer.byteLength(webValue)} / 65536 bytes`),
  ).toBeVisible();
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.getByText("Clipboard sent to remote")).toBeVisible();
  await expect.poll(readRemoteClipboard).toBe(webValue);

  // An empty panel is not a way to clear either clipboard: the remote takes
  // ownership of whatever is sent, and Copy would overwrite the local value.
  await input.fill("");
  await expect(page.getByRole("button", { name: "Send" })).toBeDisabled();
  await page.getByRole("button", { name: "Copy" }).click();
  await expect(page.getByText("Nothing to copy")).toBeVisible();
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe(remoteValue);
  await expect.poll(readRemoteClipboard).toBe(webValue);
  await input.fill(webValue);

  // Some remote clipboard bridges re-announce host-provided text when the
  // guest pastes it. That echo is not a guest copy/cut and must not travel
  // back over a newer host clipboard.
  const echoSentinel = `remotex-ui-after-send-${Date.now()}`;
  await page.evaluate(
    (text) => navigator.clipboard.writeText(text),
    echoSentinel,
  );
  const pushesBeforeEcho = remotePushes;
  setRemoteClipboard(webValue);
  await expect.poll(() => remotePushes).toBeGreaterThan(pushesBeforeEcho);
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe(echoSentinel);

  // Closing discards the reveal state; reopening fetches and conceals again.
  await page.getByRole("button", { name: "Close clipboard" }).click();
  await openClipboardPanel(page);
  const reopenedMetadata = page.getByRole("button", {
    name: "Reveal remote clipboard content",
  });
  await expect(reopenedMetadata).toContainText(`CRC32 ${crc32(webValue)}`);
  await expect(reopenedMetadata).toContainText(
    `LEN ${Buffer.byteLength(webValue)}B`,
  );
  await expect(page.getByLabel("Clipboard text")).toHaveCount(0);

  // The DOM panel is stable at the mobile breakpoint; canvas pixels remain
  // deliberately unasserted.
  await page.setViewportSize({ width: 390, height: 844 });
  const panelBox = await page.locator(".panel").boundingBox();
  expect(panelBox).not.toBeNull();
  expect(Math.round(panelBox.x)).toBe(0);
  expect(Math.round(panelBox.width)).toBe(390);
  expect(Math.round(panelBox.y + panelBox.height)).toBe(844);

  await page.getByRole("button", { name: "Close clipboard" }).click();
  await page.getByRole("button", { name: "Open menu" }).click();
  await page.getByRole("button", { name: "Switch target" }).click();
  await expect(
    page.getByRole("heading", { name: "Pick a target" }),
  ).toBeVisible();
  expect(pageErrors).toEqual([]);
});
