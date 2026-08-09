// The one window, and everything that must be true of it before a page loads.

import { join } from "node:path";
import { BrowserWindow, screen, session } from "electron";
import { documentSize, windowFrameFitting } from "./geometry.ts";
import { CLIENT_URL, isShellUrl, shellPageUrl } from "./scheme-routes.ts";
import { DEFAULT_WINDOW_SIZE, type WindowSize } from "./window-size.ts";

/**
 * How small the window may get before it stops being a desktop.
 *
 * A live-resize floor only. What a launch *opens* at is the remembered size
 * (window-size.ts), whose own floor is higher — the two are different
 * questions, and this one deliberately allows sizes the other refuses to
 * write down.
 */
export const MINIMUM_SIZE = { width: 720, height: 480 };

export function createViewerWindow(
  distDir: string,
  onPreloadError: (message: string) => void,
  openingSize: WindowSize | null,
): BrowserWindow {
  const size = openingSize ?? DEFAULT_WINDOW_SIZE;
  const window = new BrowserWindow({
    width: size.width,
    height: size.height,
    minWidth: MINIMUM_SIZE.width,
    minHeight: MINIMUM_SIZE.height,
    show: false,
    backgroundColor: "#101014",
    // macOS swallows the click that activates a window, but delivers the moves
    // before it — so an unfocused desktop tracks the cursor and then ignores the
    // click that follows it, which reads as the remote having missed it. The
    // surface is a guest's desktop: a click on it is meant for the guest, whether
    // or not this window happened to be in front.
    acceptFirstMouse: true,
    webPreferences: {
      preload: join(distDir, "bridge.cjs"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      spellcheck: false,
      // The client keeps painting and keeps its socket fed while the window is
      // behind another one. A throttled remote desktop is one that reconnects.
      backgroundThrottling: false,
    },
  });

  lockDownNavigation(window);

  // A preload that does not load is the worst failure this app has, because it does
  // not look like a failure: the client's two globals are the only thing that tells
  // it which host it is in, and without them it concludes it is in a browser and
  // draws a login screen for a gateway whose address it was never given. That is a
  // working-looking window in which nothing can work. The launch screen fails the
  // same way from the same cause — it keeps saying the gateway is starting. So it is
  // reported, loudly, rather than left to be diagnosed from the symptom.
  window.webContents.on("preload-error", (_event, path, error) => {
    onPreloadError(
      `The client bridge did not load (${path}): ${error.message}`,
    );
  });

  window.once("ready-to-show", () => window.show());
  return window;
}

/**
 * This window shows one page from one origin, and there is no way to make it show
 * anything else.
 *
 * The client never navigates: it is a single document that talks over a socket. So
 * anything that *would* navigate — a link in remote clipboard text, a redirect, a
 * `javascript:` URL typed into the inspector — is something going wrong, and the
 * safe answer to all of it is the same one.
 */
function lockDownNavigation(window: BrowserWindow): void {
  window.webContents.on("will-navigate", (event, url) => {
    if (!isShellUrl(url)) {
      event.preventDefault();
      console.warn(`remotex: refused a navigation to ${url}`);
    }
  });
  window.webContents.setWindowOpenHandler(({ url }) => {
    // Not opened here and not opened in a browser either: nothing in this app has a
    // link worth following, so a popup is a page doing something unasked.
    console.warn(`remotex: refused a new window for ${url}`);
    return { action: "deny" };
  });
  window.webContents.on("context-menu", (event) => {
    // There is no menu that would be right here: the remote surface's right-click
    // belongs to the guest, and a Reload item on a client with no router is a way to
    // lose a session by accident.
    event.preventDefault();
  });
}

/**
 * Put the launch token where the page will send it, and only then load the page.
 *
 * `SameSite=None` because the document and its gateway are genuinely different
 * sites — a `Lax` cookie is simply not sent — and `Secure` because `None` requires
 * it, which loopback is allowed to be.
 *
 * Awaited before the load, or the client's first request arrives anonymous and the
 * gateway answers 401 with nothing on the wire to explain why.
 */
export async function seedSessionCookie(
  port: number,
  token: string,
): Promise<void> {
  await session.defaultSession.cookies.set({
    url: `http://127.0.0.1:${port}`,
    name: "remotex_session",
    value: token,
    sameSite: "no_restriction",
    secure: true,
    expirationDate: Date.now() / 1000 + 365 * 24 * 3600,
  });
}

/** Forget the previous launch's token. A new gateway mints a new one. */
export async function clearSessionCookies(): Promise<void> {
  for (const cookie of await session.defaultSession.cookies.get({
    name: "remotex_session",
  })) {
    const url = `http://${cookie.domain?.replace(/^\./, "") ?? "127.0.0.1"}${
      cookie.path ?? "/"
    }`;
    await session.defaultSession.cookies.remove(url, cookie.name);
  }
}

export async function loadClient(window: BrowserWindow): Promise<void> {
  await window.loadURL(CLIENT_URL);
}

export async function loadShellPage(
  window: BrowserWindow,
  page: string,
): Promise<void> {
  await window.loadURL(shellPageUrl(page));
}

/**
 * Fit the window around the remote desktop at 100%.
 *
 * The chrome is measured, not assumed: `getContentSize` is what the page has now,
 * and the difference from the window is what the title bar and borders take. The
 * screen the window is on is the limit, so a 4K guest on a laptop gets as much of
 * itself as fits and scrolls the rest — never a scaled-down picture.
 */
export function fitWindowToRemote(
  window: BrowserWindow,
  remote: { w: number; h: number; scale: number },
): void {
  const [windowWidth, windowHeight] = window.getSize();
  const [contentWidth, contentHeight] = window.getContentSize();
  const bounds = window.getBounds();
  const display = screen.getDisplayMatching(bounds);
  const frame = windowFrameFitting(
    documentSize(remote),
    { width: contentWidth, height: contentHeight },
    { ...bounds, width: windowWidth, height: windowHeight },
    display.workArea,
    MINIMUM_SIZE,
  );
  window.setBounds(frame);
}

/**
 * Refuse every permission a page can ask for.
 *
 * The client needs none: the camera, the microphone, notifications and geolocation
 * have nothing to do with a remote desktop, and the one capability it would want —
 * the system clipboard — is the shell's, precisely because a page cannot have it.
 */
export function refuseEveryPermission(): void {
  session.defaultSession.setPermissionRequestHandler(
    (_contents, _permission, callback) => callback(false),
  );
}
