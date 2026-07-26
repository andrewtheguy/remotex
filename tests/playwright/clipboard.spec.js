const { execFileSync } = require("node:child_process");
const { test, expect } = require("@playwright/test");

const BASE_URL =
  process.env.REMOTEX_PLAYWRIGHT_BASE_URL ?? "http://127.0.0.1:5173/";
const USERNAME = process.env.REMOTEX_PLAYWRIGHT_USERNAME;
const PASSWORD = process.env.REMOTEX_PLAYWRIGHT_PASSWORD;
const TARGET = process.env.REMOTEX_PLAYWRIGHT_TARGET ?? "mac";
const MAC_SSH = process.env.REMOTEX_PLAYWRIGHT_MAC_SSH;
const REQUIRED_ENV = {
  REMOTEX_PLAYWRIGHT_USERNAME: USERNAME,
  REMOTEX_PLAYWRIGHT_PASSWORD: PASSWORD,
  REMOTEX_PLAYWRIGHT_MAC_SSH: MAC_SSH,
};
const MISSING_ENV = Object.entries(REQUIRED_ENV)
  .filter(([, value]) => !value)
  .map(([name]) => name);

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

function setRemoteClipboard(text) {
  const encoded = Buffer.from(text, "utf8").toString("base64");
  execFileSync("ssh", [
    MAC_SSH,
    `printf '%s' '${encoded}' | base64 --decode | pbcopy`,
  ]);
}

function readRemoteClipboard() {
  return execFileSync("ssh", [MAC_SSH, "pbpaste"], {
    encoding: "utf8",
  }).replace(/\r?\n$/, "");
}

function targetNamePattern(name) {
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`^${escaped}\\b`);
}

test("clipboard panel reads require explicit Copy while pushes still auto-sync", async ({
  page,
}) => {
  test.skip(
    MISSING_ENV.length > 0,
    `live Mac clipboard test requires ${MISSING_ENV.join(", ")}`,
  );

  const pageErrors = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));

  await page.goto(BASE_URL);
  await expect(page.getByText(/^v\d+\.\d+\.\d+$/)).toBeVisible();
  await page.getByLabel("Username").fill(USERNAME);
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByRole("button", { name: "Log in" }).click();

  await expect(
    page.getByRole("heading", { name: "Pick a target" }),
  ).toBeVisible();
  await page
    .getByRole("button", { name: targetNamePattern(TARGET) })
    .click();
  await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
    timeout: 20_000,
  });

  // Unsolicited remote changes retain the established automatic-sync path.
  const remoteValue = `remotex-ui-remote-${Date.now()}`;
  setRemoteClipboard(remoteValue);
  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toBe(remoteValue);

  // A panel Fetch and Reveal are reads. Neither may cross the explicit Copy
  // boundary and replace this unrelated local clipboard value.
  const localSentinel = `remotex-ui-local-${Date.now()}`;
  await page.evaluate(
    (text) => navigator.clipboard.writeText(text),
    localSentinel,
  );

  await page.getByRole("button", { name: "Open menu" }).click();
  await page.getByRole("button", { name: "Clipboard", exact: true }).click();

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

  // Closing discards the reveal state; reopening fetches and conceals again.
  await page.getByRole("button", { name: "Close clipboard" }).click();
  await page.getByRole("button", { name: "Open menu" }).click();
  await page.getByRole("button", { name: "Clipboard", exact: true }).click();
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
