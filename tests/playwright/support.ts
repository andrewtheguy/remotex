// Shared setup for the live-Mac clipboard specs: the environment they need, the
// login/target flow, and the SSH hooks that drive the Mac's own pasteboard.
//
// Both specs run against the same single session slot, so they are sequential by
// configuration (workers: 1) rather than by luck — a second browser claiming the
// slot would evict the first.
import { execFileSync } from "node:child_process";
import { expect, type Page } from "@playwright/test";

export const BASE_URL =
  process.env.REMOTEX_PLAYWRIGHT_BASE_URL ?? "http://127.0.0.1:5173/";
export const TARGET = process.env.REMOTEX_PLAYWRIGHT_TARGET ?? "mac";
const USERNAME = process.env.REMOTEX_PLAYWRIGHT_USERNAME;
const PASSWORD = process.env.REMOTEX_PLAYWRIGHT_PASSWORD;
const MAC_SSH = process.env.REMOTEX_PLAYWRIGHT_MAC_SSH;
const SSH_TIMEOUT_MS = 10_000;
const REQUIRED_ENV: Record<string, string | undefined> = {
  REMOTEX_PLAYWRIGHT_USERNAME: USERNAME,
  REMOTEX_PLAYWRIGHT_PASSWORD: PASSWORD,
  REMOTEX_PLAYWRIGHT_MAC_SSH: MAC_SSH,
};
export const MISSING_ENV = Object.entries(REQUIRED_ENV)
  .filter(([, value]) => !value)
  .map(([name]) => name);

// The three above are optional in the environment but required by the time a
// test body runs, which `test.skip(MISSING_ENV.length > 0, …)` guarantees. This
// turns that guarantee into something the types agree with, instead of a `!` on
// every use.
function required(name: string, value: string | undefined): string {
  if (!value) {
    throw new Error(`${name} is unset; MISSING_ENV should have skipped this`);
  }
  return value;
}

export function setRemoteClipboard(text: string): void {
  const encoded = Buffer.from(text, "utf8").toString("base64");
  execFileSync(
    "ssh",
    [
      required("REMOTEX_PLAYWRIGHT_MAC_SSH", MAC_SSH),
      `printf '%s' '${encoded}' | base64 --decode | pbcopy`,
    ],
    { timeout: SSH_TIMEOUT_MS },
  );
}

// A pasteboard of `bytes` ASCII characters, generated on the Mac rather than
// sent over SSH: the point of this one is a size the link is meant to refuse.
export function setRemoteClipboardBytes(bytes: number): void {
  execFileSync(
    "ssh",
    [
      required("REMOTEX_PLAYWRIGHT_MAC_SSH", MAC_SSH),
      `python3 -c 'print("x"*${bytes}, end="")' | pbcopy`,
    ],
    { timeout: SSH_TIMEOUT_MS },
  );
}

export function readRemoteClipboard(): string {
  return execFileSync(
    "ssh",
    [required("REMOTEX_PLAYWRIGHT_MAC_SSH", MAC_SSH), "pbpaste"],
    { encoding: "utf8", timeout: SSH_TIMEOUT_MS },
  ).replace(/\r?\n$/, "");
}

export function targetNamePattern(name: string): RegExp {
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
export async function logInAndConnect(page: Page): Promise<void> {
  await page.goto(BASE_URL);
  await expect(page.getByText(/^v\d+\.\d+\.\d+$/)).toBeVisible();
  await page
    .getByLabel("Username")
    .fill(required("REMOTEX_PLAYWRIGHT_USERNAME", USERNAME));
  await page
    .getByLabel("Password")
    .fill(required("REMOTEX_PLAYWRIGHT_PASSWORD", PASSWORD));
  await page.getByRole("button", { name: "Log in" }).click();

  const picker = page.getByRole("heading", { name: "Pick a target" });
  const menu = page.getByRole("button", { name: "Open menu" });
  await expect(picker.or(menu).first()).toBeVisible({ timeout: 20_000 });
  if (await picker.isVisible()) {
    await page.getByRole("button", { name: targetNamePattern(TARGET) }).click();
  }
  await expect(menu).toBeVisible({ timeout: 20_000 });
}

export async function openClipboardPanel(page: Page): Promise<void> {
  await page.getByRole("button", { name: "Open menu" }).click();
  await page.getByRole("button", { name: "Clipboard", exact: true }).click();
}

// Hand the session back to the picker, so the next spec starts where this one
// did. Every spec here ends with this for that reason.
export async function returnToPicker(page: Page): Promise<void> {
  await page.getByRole("button", { name: "Open menu" }).click();
  await page.getByRole("button", { name: "Switch target" }).click();
  await expect(
    page.getByRole("heading", { name: "Pick a target" }),
  ).toBeVisible();
}
