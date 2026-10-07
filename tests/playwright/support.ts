// Shared setup for the live-Mac clipboard specs: the environment they need, the
// login/target flow, and the SSH hooks that drive the Mac's own pasteboard.
//
// Both specs run against the same single session slot, so they are sequential by
// configuration (workers: 1) rather than by luck — a second browser claiming the
// slot would evict the first.
import { execFileSync } from "node:child_process";
import { expect, type Page, test } from "@playwright/test";

export const BASE_URL =
  process.env.REMOTEX_PLAYWRIGHT_BASE_URL ?? "http://127.0.0.1:52380/";
export const TARGET = process.env.REMOTEX_PLAYWRIGHT_TARGET ?? "mac";
const USERNAME = process.env.REMOTEX_PLAYWRIGHT_USERNAME;
const PASSWORD = process.env.REMOTEX_PLAYWRIGHT_PASSWORD;
const MAC_SSH = process.env.REMOTEX_PLAYWRIGHT_MAC_SSH;
const SSH_TIMEOUT_MS = 10_000;
const LEAVE_TIMEOUT_MS = 10_000;
const REQUIRED_ENV: Record<string, string | undefined> = {
  REMOTEX_PLAYWRIGHT_USERNAME: USERNAME,
  REMOTEX_PLAYWRIGHT_PASSWORD: PASSWORD,
  REMOTEX_PLAYWRIGHT_MAC_SSH: MAC_SSH,
};
export const MISSING_ENV = Object.entries(REQUIRED_ENV)
  .filter(([, value]) => !value)
  .map(([name]) => name);

/// Where the Mac's Screen Sharing service listens, as `host:port` — and the opt-in
/// for every spec that needs a live Mac target.
///
/// An endpoint rather than a flag, because it is the specific thing those specs
/// depend on and the only thing that can be checked: credentials do not tell you a
/// VM is up, and with the Mac merely stopped the specs failed on a
/// connect timeout deep inside the browser, reading as a bug in whatever was being
/// changed. Unset by default, so a plain run never assumes a live Mac — the
/// browser-side equivalent of `#[ignore]` on the Rust e2e tests that need a
/// container.
///
///     REMOTEX_PLAYWRIGHT_MAC_SCREEN_SHARING=... npx playwright test
export const SCREEN_SHARING_ENDPOINT =
  process.env.REMOTEX_PLAYWRIGHT_MAC_SCREEN_SHARING;

/// Whether something is listening at [`SCREEN_SHARING_ENDPOINT`].
///
/// A TCP connect is sufficient to establish that Screen Sharing is reachable.
/// `nc`, because this has to answer synchronously for a `test.skip` at suite level,
/// and this file already shells out for the pasteboard. A probe that cannot run at
/// all (no `nc`) answers `true`: the point is to skip a *known* absent service, not
/// to guess at one.
///
/// `-G`, the connect timeout, is macOS's `nc` alone: anywhere else it is an option
/// `nc` does not know, which exits non-zero like a refused connection and skipped
/// every live-Mac spec. There `-w` bounds the connect as well.
function screenSharingIsListening(endpoint: string): boolean {
  const at = endpoint.lastIndexOf(":");
  const host = at > 0 ? endpoint.slice(0, at) : endpoint;
  const port = at > 0 ? endpoint.slice(at + 1) : "";
  const connectTimeout = process.platform === "darwin" ? ["-G", "2"] : [];
  try {
    execFileSync("nc", ["-z", ...connectTimeout, "-w", "2", host, port], {
      stdio: "ignore",
      timeout: SSH_TIMEOUT_MS,
    });
    return true;
  } catch (cause) {
    // `nc` exits non-zero for a refused or timed-out connection, which is the
    // answer. Anything without an exit status is `nc` itself missing, which is not.
    return !(cause instanceof Error && "status" in cause);
  }
}

/// Skip the enclosing spec or suite unless a live Mac target was requested, its
/// Screen Sharing service can be reached, and the environment to drive it is set.
/// One place for all three, so every such spec skips for the same reasons and says
/// which one applied.
export function skipUnlessLiveMac(): void {
  if (!SCREEN_SHARING_ENDPOINT) {
    test.skip(
      true,
      "set REMOTEX_PLAYWRIGHT_MAC_SCREEN_SHARING=host:port to run the live-Mac specs",
    );
    return;
  }
  if (MISSING_ENV.length > 0) {
    test.skip(true, `set ${MISSING_ENV.join(", ")} to run the live-Mac specs`);
    return;
  }
  test.skip(
    !screenSharingIsListening(SCREEN_SHARING_ENDPOINT),
    `nothing is listening at ${SCREEN_SHARING_ENDPOINT} — start Screen Sharing on the Mac`,
  );
}

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
//
// Built with `head`, `tr` and `pbcopy` — no interpreter. This used to ask
// `python3` for the string, which on a Mac without the Xcode command line tools
// is a stub that prints the install notice to stderr, writes nothing, and
// *exits zero*: the pasteboard kept whatever it already held, and the spec
// failed twenty seconds later waiting for a card about a value that had never
// been set. A QA Mac should not need a toolchain to hold a string.
export function setRemoteClipboardBytes(bytes: number): void {
  execFileSync(
    "ssh",
    [
      required("REMOTEX_PLAYWRIGHT_MAC_SSH", MAC_SSH),
      `head -c ${bytes} /dev/zero | tr '\\0' 'x' | pbcopy`,
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

// What a spec starts its session with: the options chosen under the target at the
// picker before Start. Each defaults to off, and every row the target shows is set
// to what is asked rather than left as found, so a run does not depend on what an
// earlier one left remembered in the browser.
//
// `resize` is the desktop following this window. Off, the session takes the size
// the target keeps where the picker offers one; a target with no size configured
// follows the window whatever is asked, since that is the one size it has here.
export interface StartChoices {
  resize?: boolean;
  sound?: boolean;
  passthrough?: boolean;
}

// A picker button reads "<name> <protocol> · <host>", so a target is named by what
// its button starts with — up to the first space, and no further.
//
// `\b` was wrong for that: a word boundary sits between "video" and a hyphen too,
// so a config holding `video` and `video-motion`
// matched two buttons and every click failed on strict mode. Whitespace is the
// separator the button actually uses, and it is the one a name can never contain.
export function targetNamePattern(name: string): RegExp {
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`^${escaped}(?!\\S)`);
}

// Log in and get to a live desktop, which the page says in two steps, neither of
// them about canvas pixels: the floating menu's button, once the gateway has given
// this browser the session, and the end of "Connecting to the remote desktop…", once
// the gateway has reached the remote and announced its desktop.
//
// The second is what a spec that then acts on the remote needs. The button alone
// left a window of a second or so in which the gateway was still logging on, and a
// value put on a Mac's pasteboard inside it was there before the pasteboard was
// watched: not a change, never announced, and the spec waited for it in vain.
//
// Either landing is accepted, because the server keeps a target session running
// when its browser goes away: a run that ended on the desktop — or crashed there
// — is reattached straight to it and never sees the picker. Requiring the picker
// here made one abandoned run break every run after it.
export async function logInAndConnect(
  page: Page,
  choices: StartChoices = {},
): Promise<void> {
  await landOn(page, TARGET, true, "", choices);
}

// Log in and land on *one named target*, whichever the run started on.
//
// The tolerance above is what makes an abandoned run harmless, and it is exactly
// what a spec about a particular target cannot have: reattaching to whatever was
// left running would assert against the wrong dial and read as a product failure.
// So a session found on a desktop is handed back to the picker first, and the
// target is then chosen by name. `search` is the page's query at load, for a spec
// about something the page decides from its URL. `choices` is what its session is
// started with.
export async function logInAndConnectTo(
  page: Page,
  target: string,
  search = "",
  choices: StartChoices = {},
): Promise<void> {
  await landOn(page, target, false, search, choices);
}

// The size that follows this browser's window, as the picker names it.
export const FOLLOWS_WINDOW = /This window's size/;

// Open `target` at the picker, set its options to `choices` and press Start.
//
// The target's button opens it rather than connecting, and says whether it is open:
// one left open by an earlier Start on this picker would be closed by a second
// click. An
// option the target does not show is left alone when it is not asked for; one that
// is asked for and is absent or greyed fails here, by name, rather than as a
// session that started without it.
async function startTarget(
  page: Page,
  target: string,
  choices: StartChoices,
): Promise<void> {
  const row = page.getByRole("button", { name: targetNamePattern(target) });
  if ((await row.getAttribute("aria-expanded")) !== "true") {
    await row.click();
  }
  const item = page.getByRole("listitem").filter({ has: row });
  // The size is a pair of radio buttons where the target offers two, the kept size
  // first, and one stated line otherwise.
  const follows = item.getByRole("radio", { name: FOLLOWS_WINDOW });
  if ((await follows.count()) > 0) {
    const size = choices.resize
      ? follows
      : item.getByRole("group", { name: "Size" }).getByRole("radio").first();
    await size.check({ timeout: LEAVE_TIMEOUT_MS });
  } else if (choices.resize) {
    await expect(item).toContainText(FOLLOWS_WINDOW);
  }
  // Sound is ticked or not, where the target offers it: asked for, it is Opus.
  const sound = item.getByRole("checkbox", { name: /^Sound/ });
  if (choices.sound || (await sound.count()) > 0) {
    await sound.setChecked(choices.sound ?? false, {
      timeout: LEAVE_TIMEOUT_MS,
    });
  }
  if (choices.sound) {
    await item
      .getByRole("radio", { name: /^Opus/ })
      .check({ timeout: LEAVE_TIMEOUT_MS });
  }
  const passed = item.getByRole("checkbox", { name: /^Pass / });
  const wanted = choices.passthrough ?? false;
  if (wanted || ((await passed.count()) > 0 && (await passed.isEnabled()))) {
    await passed.setChecked(wanted, { timeout: LEAVE_TIMEOUT_MS });
  }
  await item.getByRole("button", { name: "Start", exact: true }).click();
}

// Both of the above, differing only in what they do about a session that is already
// on a desktop: `keepRunningSession` reattaches to it, and otherwise it is handed
// back to the picker so `target` is the one actually connected. One implementation,
// so the locators and the timeout cannot drift apart between them.
async function landOn(
  page: Page,
  target: string,
  keepRunningSession: boolean,
  search: string,
  choices: StartChoices,
): Promise<void> {
  await logIn(page, search);
  const onPicker = await page
    .getByRole("heading", { name: "Pick a target" })
    .isVisible();
  // A landing on a desktop is either kept — the tolerance that makes an abandoned run
  // harmless — or handed back to the picker, so the target chosen below is the one
  // actually connected.
  if (onPicker || !keepRunningSession) {
    if (!onPicker) {
      await returnToPicker(page);
    }
    await startTarget(page, target, choices);
  }
  await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
    timeout: 20_000,
  });
  await expect(page.getByText("Connecting to the remote desktop…")).toBeHidden({
    timeout: 20_000,
  });
}

// The login itself, which ends on whichever of the two landings this run gets.
export async function logIn(page: Page, search = ""): Promise<void> {
  await page.goto(new URL(search, BASE_URL).toString());
  await expect(page.getByText(/^v\d+\.\d+\.\d+$/)).toBeVisible();
  await page
    .getByLabel("Username")
    .fill(required("REMOTEX_PLAYWRIGHT_USERNAME", USERNAME));
  await page
    .getByLabel("Password")
    .fill(required("REMOTEX_PLAYWRIGHT_PASSWORD", PASSWORD));
  await page.getByRole("button", { name: "Log in" }).click();

  // A third landing, and the one that used to end a run before it started: the slot
  // is held by a browser that is gone or busy — a previous test's context, a QA tab
  // left open — and the page offers the takeover rather than deciding for anybody.
  // Taking it is what a test run wants and is the flow the product documents: it
  // ends whatever session was there, and this page lands on the picker.
  const takeOver = page.getByRole("button", { name: "Take over" });
  const picker = page.getByRole("heading", { name: "Pick a target" });
  const menu = page.getByRole("button", { name: "Open menu" });
  await expect(takeOver.or(picker).or(menu).first()).toBeVisible({
    timeout: 20_000,
  });
  if (await takeOver.isVisible()) {
    await takeOver.click();
  }
  await expect(picker.or(menu).first()).toBeVisible({ timeout: 20_000 });
}

export async function openClipboardPanel(page: Page): Promise<void> {
  await page.getByRole("button", { name: "Open menu" }).click();
  await page.getByRole("button", { name: "Clipboard", exact: true }).click();
}

// Hand the session back to the picker, so the next spec starts where this one
// did. Every spec here does this on the way out, most of them through
// `leaveSession` below.
//
// The clicks have a timeout of their own. Without one a button that cannot be
// clicked — under a panel a spec left open — is retried until the test's timeout,
// and then again for as long under the `afterEach` that cleans up.
export async function returnToPicker(page: Page): Promise<void> {
  await page
    .getByRole("button", { name: "Open menu" })
    .click({ timeout: LEAVE_TIMEOUT_MS });
  await page
    .getByRole("button", { name: "End session" })
    .click({ timeout: LEAVE_TIMEOUT_MS });
  await expect(
    page.getByRole("heading", { name: "Pick a target" }),
  ).toBeVisible();
}

// The same thing as cleanup, for an `afterEach`: hand the session back even when a
// spec threw halfway, so a failing run does not leave the gateway's one slot sitting
// on a desktop for the next spec to take over.
//
// Every step is conditional, because cleanup runs after failures and a hook that
// threw would bury the real one under a second. The drawer is closed first for the
// reason `returnToPicker` opens it: the toggle is one button that reads "Close menu"
// while the drawer is open, so a spec that failed with it open would send the click
// below looking for a button that is not there. A panel is closed too: it is a
// sheet along the bottom of the window, which the toggle can be under, and a spec
// that failed with one open would leave the click below nothing to land on.
export async function leaveSession(page: Page): Promise<void> {
  const drawer = page.getByRole("button", { name: "Close menu" });
  if (await drawer.isVisible()) {
    await drawer.click({ timeout: LEAVE_TIMEOUT_MS });
  }
  const panel = page.getByRole("button", {
    name: /^Close (clipboard|display picker|soft keyboard)$/,
  });
  if (await panel.first().isVisible()) {
    await panel.first().click({ timeout: LEAVE_TIMEOUT_MS });
  }
  if (await page.getByRole("button", { name: "Open menu" }).isVisible()) {
    await returnToPicker(page);
  }
}

// Choose `display` in the session page's Display picker, from the drawer's button
// for the display shown now, and leave the drawer and the picker closed whichever
// of them the choice left open.
export async function chooseDisplay(
  page: Page,
  shown: string,
  display: string,
): Promise<void> {
  await page.getByRole("button", { name: "Open menu" }).click();
  await page.getByRole("button", { name: shown, exact: true }).click();
  // The picker's entries are pressed buttons, one pressed at a time; the one
  // asked for is not it yet.
  await page.getByRole("button", { name: display, pressed: false }).click();
  const picker = page.getByRole("button", { name: "Close display picker" });
  if (await picker.isVisible()) {
    await picker.click();
  }
  const drawer = page.getByRole("button", { name: "Close menu" });
  if (await drawer.isVisible()) {
    await drawer.click();
  }
}
