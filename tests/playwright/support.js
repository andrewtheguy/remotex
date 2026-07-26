// Shared setup for the live-Mac clipboard specs: the environment they need, the
// login/target flow, and the SSH hooks that drive the Mac's own pasteboard.
//
// Both specs run against the same single session slot, so they are sequential by
// configuration (workers: 1) rather than by luck — a second browser claiming the
// slot would evict the first.
const { execFileSync } = require("node:child_process");
const { expect } = require("@playwright/test");

const BASE_URL =
  process.env.REMOTEX_PLAYWRIGHT_BASE_URL ?? "http://127.0.0.1:5173/";
const USERNAME = process.env.REMOTEX_PLAYWRIGHT_USERNAME;
const PASSWORD = process.env.REMOTEX_PLAYWRIGHT_PASSWORD;
const TARGET = process.env.REMOTEX_PLAYWRIGHT_TARGET ?? "mac";
const MAC_SSH = process.env.REMOTEX_PLAYWRIGHT_MAC_SSH;
const SSH_TIMEOUT_MS = 10_000;
const REQUIRED_ENV = {
  REMOTEX_PLAYWRIGHT_USERNAME: USERNAME,
  REMOTEX_PLAYWRIGHT_PASSWORD: PASSWORD,
  REMOTEX_PLAYWRIGHT_MAC_SSH: MAC_SSH,
};
const MISSING_ENV = Object.entries(REQUIRED_ENV)
  .filter(([, value]) => !value)
  .map(([name]) => name);

function setRemoteClipboard(text) {
  const encoded = Buffer.from(text, "utf8").toString("base64");
  execFileSync(
    "ssh",
    [MAC_SSH, `printf '%s' '${encoded}' | base64 --decode | pbcopy`],
    {
      timeout: SSH_TIMEOUT_MS,
    },
  );
}

// A pasteboard of `bytes` ASCII characters, generated on the Mac rather than
// sent over SSH: the point of this one is a size the link is meant to refuse.
function setRemoteClipboardBytes(bytes) {
  execFileSync(
    "ssh",
    [MAC_SSH, `python3 -c 'print("x"*${bytes}, end="")' | pbcopy`],
    { timeout: SSH_TIMEOUT_MS },
  );
}

function readRemoteClipboard() {
  return execFileSync("ssh", [MAC_SSH, "pbpaste"], {
    encoding: "utf8",
    timeout: SSH_TIMEOUT_MS,
  }).replace(/\r?\n$/, "");
}

function targetNamePattern(name) {
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`^${escaped}\\b`);
}

// Log in and get to a live desktop, which the floating menu's button proves
// without asserting anything about canvas pixels.
//
// Either landing is accepted, because the server keeps a target session running
// when its browser goes away: a run that ended on the desktop — or crashed there
// — is reattached straight to it and never sees the picker. Requiring the picker
// here made one abandoned run break every run after it.
async function logInAndConnect(page) {
  await page.goto(BASE_URL);
  await expect(page.getByText(/^v\d+\.\d+\.\d+$/)).toBeVisible();
  await page.getByLabel("Username").fill(USERNAME);
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByRole("button", { name: "Log in" }).click();

  const picker = page.getByRole("heading", { name: "Pick a target" });
  const menu = page.getByRole("button", { name: "Open menu" });
  await expect(picker.or(menu).first()).toBeVisible({ timeout: 20_000 });
  if (await picker.isVisible()) {
    await page.getByRole("button", { name: targetNamePattern(TARGET) }).click();
  }
  await expect(menu).toBeVisible({ timeout: 20_000 });
}

// Hand the session back to the picker, so the next spec starts where this one
// did. Every spec here ends with this for that reason.
async function returnToPicker(page) {
  await page.getByRole("button", { name: "Open menu" }).click();
  await page.getByRole("button", { name: "Switch target" }).click();
  await expect(
    page.getByRole("heading", { name: "Pick a target" }),
  ).toBeVisible();
}

async function openClipboardPanel(page) {
  await page.getByRole("button", { name: "Open menu" }).click();
  await page.getByRole("button", { name: "Clipboard", exact: true }).click();
}

module.exports = {
  BASE_URL,
  MISSING_ENV,
  TARGET,
  logInAndConnect,
  openClipboardPanel,
  readRemoteClipboard,
  returnToPicker,
  setRemoteClipboard,
  setRemoteClipboardBytes,
  targetNamePattern,
};
