// remotex.app: a window, a menu bar, a pasteboard, and the gateway underneath them.
//
// The app is a *shell*. Everything below the title bar is the client — the same
// build a browser loads, talking to the same gateway over the same socket — and this
// process holds no session, no claim and no wire format. What it adds is the handful
// of things a page genuinely cannot do for itself:
//
//   - every ⌘ chord, ⌘Q and ⌘W included, reaching the guest;
//   - a clipboard that keeps syncing while the window is unfocused;
//   - a real menu bar, and sound started from it with no gesture in the page;
//   - a gateway of its own, started and stopped with the app.
//
// The launch sequence, in the order it has to happen: read the instance directory
// out of argv, point the profile at it, declare the scheme, become ready, start the
// gateway, take the token it prints, put it in the cookie jar, and only then load
// the page.

import { existsSync, mkdirSync, readFileSync } from "node:fs";
import { basename, join, resolve } from "node:path";
import {
  app,
  BrowserWindow,
  clipboard,
  dialog,
  ipcMain,
  screen,
  shell,
} from "electron";
import {
  CHANNEL,
  type ConfigSaveResult,
  type NativeCommand,
  type NativeEvent,
  type ShellAction,
  type ShellStatus,
} from "../shared/contract.ts";
import { ClipboardSynchronizer, POLL_INTERVAL_MS } from "./clipboard.ts";
import { ConfigStore, validatorOverBinary } from "./config-store.ts";
import { EmbeddedGateway, LaunchFailure } from "./gateway.ts";
import {
  type InstanceDirectory,
  instanceAt,
  instanceDirFromArgv,
} from "./instance.ts";
import type { LocalAction } from "./menu.ts";
import { installMenu } from "./menu-install.ts";
import { distDirFor, resourceLayout } from "./paths.ts";
import { declareShellScheme, serveShellScheme } from "./scheme.ts";
import { shellPageUrl } from "./scheme-routes.ts";
import {
  createShimBundle,
  instanceDirFromBundle,
  mainBundleFor,
  type PlistReader,
} from "./shim.ts";
import {
  acceptState,
  blankState,
  clipboardShouldFollow,
  guestOwnsKeyboard,
  type Screen,
  type ViewerState,
  windowTitle,
} from "./state.ts";
import {
  clearSessionCookies,
  createViewerWindow,
  fitWindowToRemote,
  loadClient,
  loadShellPage,
  refuseEveryPermission,
  seedSessionCookie,
} from "./window.ts";

/** Stamped in at build time from `Cargo.toml`'s workspace version. */
declare const __VIEWER_VERSION__: string;

// Deliberately not `__dirname` — see `distDirFor`, which is where that story is.
const distDir = distDirFor(app.getAppPath());
// `<repo>/apps/viewer/dist` → `<repo>`, which is where a development run finds the
// gateway and the client it was told nothing else about.
const appRoot = app.isPackaged
  ? process.resourcesPath
  : resolve(distDir, "../../..");
const layout = resourceLayout(app.isPackaged, appRoot, distDir);

// --- Instance and profile -------------------------------------------------
//
// Order matters and is not obvious: Electron's default `userData` is already
// `~/Library/Application Support/<CFBundleName>`, which is the instance directory a
// double-clicked app should use — so it is read *before* being overridden, and both
// happen before anything asks where the profile is.
//
// Between the flag and that default sits the bundle's own Info.plist: an instance
// app made by **New Instance App…** is launched by LaunchServices with no arguments
// at all, and the directory it exists to open rides in its plist the way Chrome's
// shims carry `CrAppModeUserDataDir`. The flag still wins, so a shim can be QA'd
// like anything else.
const readPlist: PlistReader = (path) => {
  try {
    return readFileSync(path, "utf8");
  } catch {
    return null;
  }
};
const instanceDir =
  instanceDirFromArgv(process.argv, process.cwd(), undefined, (message) =>
    console.warn(message),
  ) ??
  (app.isPackaged
    ? instanceDirFromBundle(process.execPath, readPlist)
    : null) ??
  app.getPath("userData");
const instance: InstanceDirectory = instanceAt(instanceDir);
// `0700` because `remotex.toml` holds the credentials of every machine this app can
// reach: the file is written `0600`, but a directory anyone can list is a directory
// anyone can watch for a replacement.
mkdirSync(instance.dir, { recursive: true, mode: 0o700 });
app.setPath("userData", instance.profileDir);

// Two launches of the same instance are one window; two instance directories are two
// apps that share nothing. Taken after the override above, so the lock lives in the
// instance's own profile.
if (!app.requestSingleInstanceLock()) {
  // `exit`, not `quit`: this is module scope, and `quit` only *asks* — the lines
  // below would run anyway, so the loser would declare the scheme, start a second
  // gateway on the winner's instance directory and open a window before its request
  // to quit was ever honoured.
  app.exit(0);
}

// --- Chromium switches ----------------------------------------------------
//
// Wholesale replaceable from the environment, because "is this switch the thing?"
// should be one relaunch away rather than a rebuild.
const DEFAULT_SWITCHES: [string, string?][] = [
  // Chromium ≥ 151 *hangs* a request from a public origin — which `remotex://app`
  // is — to 127.0.0.1, with no error anywhere: a black window and an empty `#root`.
  // Inert on a build without the feature, which is why it stays either way.
  [
    "disable-features",
    "LocalNetworkAccessChecks,LocalNetworkAccessChecksWebSockets",
  ],
  // Sound is started from a menu item, which is not a gesture in the page. Without
  // this the client's `AudioContext` comes up suspended and nothing says so.
  ["autoplay-policy", "no-user-gesture-required"],
  // Otherwise macOS asks for the keychain, in front of a window that has not
  // painted, for a cookie jar whose only content is a token this launch minted.
  ["use-mock-keychain"],
  ["password-store", "basic"],
  // Pointer clients show desktops at 100%; a pinch that scaled the canvas would make
  // the pixel the client reports and the pixel the user touched two different things.
  ["disable-pinch"],
];

for (const [name, value] of parseSwitches(
  process.env.REMOTEX_CHROMIUM_SWITCHES,
)) {
  app.commandLine.appendSwitch(name, value);
}

function parseSwitches(override: string | undefined): [string, string?][] {
  if (override === undefined) {
    return DEFAULT_SWITCHES;
  }
  return override
    .split(/\s+/)
    .filter((raw) => raw.startsWith("--"))
    .map((raw) => {
      const [name, ...rest] = raw.slice(2).split("=");
      return [name, rest.length > 0 ? rest.join("=") : undefined] as [
        string,
        string?,
      ];
    });
}

declareShellScheme();

// --- The pieces -----------------------------------------------------------

const gateway = new EmbeddedGateway(instance, {
  binary: layout.binary,
  webRoot: layout.webRoot,
});
const config = new ConfigStore(instance, validatorOverBinary(layout.binary));

let window: BrowserWindow | null = null;
let endpoint: { port: number; token: string } | null = null;
let restarting = false;
const viewer: ViewerState = {
  screen: { phase: "launching" },
  state: blankState(),
};
let failure: { message: string; log: string } | null = null;

const pasteboard = new ClipboardSynchronizer({
  pasteboard: {
    read: () => clipboard.readText(),
    write: (text) => clipboard.writeText(text),
  },
  send: (text) => sendCommand({ type: "clipboardLocal", text }),
  schedule: (tick) => {
    const timer = setInterval(tick, POLL_INTERVAL_MS);
    return () => clearInterval(timer);
  },
});

function sendCommand(command: NativeCommand): void {
  if (viewer.screen.phase === "ready") {
    window?.webContents.send(CHANNEL.command, command);
  }
}

function setScreen(next: Screen): void {
  viewer.screen = next;
  if (next.phase === "launching") {
    viewer.state = blankState();
  }
  refresh();
}

/**
 * Push everything derived at the things that show it.
 *
 * One function, called after anything that could have changed the answer, because
 * the menu bar, the window title, the keyboard rule and the clipboard are all
 * functions of the same two values — and updating them from separate places is how
 * they come to disagree.
 */
function refresh(): void {
  installMenu(
    {
      viewer,
      hostScale: window
        ? screen.getDisplayMatching(window.getBounds()).scaleFactor
        : 1,
      hasGateway: true,
      idle: !restarting,
      canMakeInstanceApps: app.isPackaged,
    },
    { send: sendCommand, local: runLocalAction },
  );
  if (window) {
    window.setTitle(windowTitle(viewer));
    // The one input decision this app makes: while a live desktop has focus its menu
    // shortcuts are suppressed, so ⌘Q, ⌘W and ⌘T reach the page as ordinary key
    // events and the client forwards them to the guest. A caret in a panel's text
    // box hands them straight back — that is what `editing` is for.
    window.webContents.setIgnoreMenuShortcuts(guestOwnsKeyboard(viewer));
  }
  pasteboard.update(clipboardShouldFollow(viewer));
  if (viewer.screen.phase === "launching") {
    postShellStatus();
  }
}

function postShellStatus(): void {
  const status: ShellStatus = failure
    ? {
        phase: "failed",
        branding: viewer.state.branding,
        message: failure.message,
        log: failure.log,
      }
    : { phase: "starting", branding: viewer.state.branding };
  window?.webContents.send(CHANNEL.shellStatus, status);
}

function runLocalAction(action: LocalAction): void {
  switch (action) {
    case "about":
      app.showAboutPanel();
      break;
    case "quit":
      app.quit();
      break;
    case "configure":
      void openConfiguration();
      break;
    case "createInstanceApp":
      void createInstanceApp();
      break;
    case "restartGateway":
      void relaunch();
      break;
    case "devTools":
      window?.webContents.openDevTools({ mode: "detach" });
      break;
    case "resizeToDisplay":
      if (window && viewer.state.size) {
        fitWindowToRemote(window, viewer.state.size);
      }
      break;
  }
}

// --- The gateway's lifecycle ----------------------------------------------

async function launch(): Promise<void> {
  failure = null;
  setScreen({ phase: "launching" });
  await loadShellPage(getWindow(), "launch.html");
  postShellStatus();

  try {
    config.bootstrapIfNeeded();
  } catch (error) {
    return report(
      LaunchFailure.instanceUnavailable(
        error instanceof Error ? error.message : String(error),
      ),
    );
  }

  gateway.onUnexpectedExit = (died) => {
    // A gateway that dies while a desktop is up is the same situation as one that
    // would not start, and the same screen says so.
    report(died);
  };

  try {
    const handshake = await gateway.start();
    endpoint = handshake;
    await clearSessionCookies();
    await seedSessionCookie(handshake.port, handshake.token);
    setScreen({ phase: "ready" });
    await loadClient(getWindow());
  } catch (error) {
    report(error);
  }
}

function report(error: unknown): void {
  const launchFailure =
    error instanceof LaunchFailure
      ? error
      : new LaunchFailure("notStarted", String(error));
  void showLaunchScreenWith(launchFailure).catch((second) => {
    // The screen that says what went wrong is itself a page load, and it can fail
    // too. There is nothing left to show it on, so it goes where a launch failure
    // already goes: `gateway.log`'s neighbour, the terminal.
    console.error("remotex: could not show the launch screen:", second);
  });
}

async function showLaunchScreenWith(error: LaunchFailure): Promise<void> {
  failure = { message: error.message, log: error.log || gateway.log() };
  endpoint = null;
  setScreen({ phase: "launching" });
  await loadShellPage(getWindow(), "launch.html");
  postShellStatus();
}

/**
 * Start over: stop the gateway, start it again, load the page again.
 *
 * What a saved config change and the launch screen's **Try Again** both do. The
 * session goes with it — the gateway is the session's ground, and a new process has
 * no memory of the old one's slot, nor of the token that opened it.
 */
async function relaunch(): Promise<void> {
  if (restarting) {
    return;
  }
  restarting = true;
  refresh();
  try {
    await gateway.stop();
    await launch();
  } finally {
    restarting = false;
    refresh();
  }
}

// --- The configuration editor ---------------------------------------------

let configWindow: BrowserWindow | null = null;

async function openConfiguration(): Promise<void> {
  if (configWindow) {
    configWindow.focus();
    return;
  }
  const parent = getWindow();
  configWindow = new BrowserWindow({
    parent,
    modal: false,
    width: 760,
    height: 620,
    title: "Configuration",
    show: false,
    backgroundColor: "#101014",
    webPreferences: {
      preload: join(distDir, "bridge.cjs"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });
  configWindow.once("ready-to-show", () => configWindow?.show());
  configWindow.on("closed", () => {
    configWindow = null;
  });
  await configWindow.loadURL(shellPageUrl("config.html"));
}

// --- Instance apps ----------------------------------------------------------

/**
 * Chrome's "Create shortcut…", for instances.
 *
 * One question — which folder is the instance — and the rest is derived: the app is
 * named after the folder, lands in `~/Applications/remotex Apps.localized`, and
 * wears `instance-icon.icns` unless the folder holds an `icon.icns` of its own.
 * The shim borrows from the *main* bundle even when this launch is itself a shim,
 * so shims never chain through each other.
 */
async function createInstanceApp(): Promise<void> {
  const owner = window ?? getWindow();
  const picked = await dialog.showOpenDialog(owner, {
    title: "New Instance App",
    message:
      "Choose the folder that holds the new instance — its configuration, log " +
      "and profile live there. An icon.icns inside it becomes the app's icon.",
    buttonLabel: "Create App",
    properties: ["openDirectory", "createDirectory"],
  });
  if (picked.canceled || picked.filePaths.length === 0) {
    return;
  }
  const dir = resolve(picked.filePaths[0]);
  if (dir === resolve(instance.dir)) {
    await dialog.showMessageBox(owner, {
      type: "error",
      message: "That folder is this launch's own instance.",
      detail:
        "An instance app exists to open a different instance beside this one. " +
        "Choose or create another folder.",
    });
    return;
  }
  try {
    const mainBundle = mainBundleFor(process.execPath, readPlist);
    const ownIcon = join(dir, "icon.icns");
    const bundle = await createShimBundle({
      instanceDir: dir,
      name: basename(dir),
      appsDir: join(
        app.getPath("home"),
        "Applications",
        "remotex Apps.localized",
      ),
      mainBundle,
      icon: existsSync(ownIcon)
        ? ownIcon
        : join(mainBundle, "Contents", "Resources", "instance-icon.icns"),
    });
    // Revealed rather than announced, which is also what registers the new bundle
    // with LaunchServices before anyone goes looking for it in Spotlight.
    shell.showItemInFolder(bundle);
  } catch (error) {
    await dialog.showMessageBox(owner, {
      type: "error",
      message: "The instance app could not be created.",
      detail: error instanceof Error ? error.message : String(error),
    });
  }
}

// --- Wiring ---------------------------------------------------------------

function getWindow(): BrowserWindow {
  if (!window) {
    window = createViewerWindow(distDir, (message) => {
      // Straight to the launch screen: a client that cannot reach its shell is not
      // a client, and the alternative is a login form for a gateway nobody named.
      report(new LaunchFailure("clientMissing", message));
    });
    window.on("closed", () => {
      window = null;
    });
    // A window that is not focused is not one the guest is typing at, so the menu
    // gets its shortcuts back until it is.
    window.on("focus", refresh);
    window.on("blur", () => {
      window?.webContents.setIgnoreMenuShortcuts(false);
    });
  }
  return window;
}

ipcMain.on(CHANNEL.gateway, (event) => {
  // Synchronous, from the client's preload: `gateway.ts` reads this at module load.
  event.returnValue = endpoint ? `http://127.0.0.1:${endpoint.port}` : "";
});

ipcMain.on(CHANNEL.event, (_event, reported: NativeEvent) => {
  switch (reported?.type) {
    case "state":
      viewer.state = acceptState(reported.state);
      refresh();
      break;
    case "clipboardFromRemote":
      pasteboard.receiveFromRemote(reported.text);
      break;
    case "unauthenticated":
      // The gateway did not take the token this app minted for it. Nothing on the
      // page can fix that — there is no login here — so the app takes the screen
      // back and offers the restart that mints a new one.
      report(
        new LaunchFailure(
          "refused",
          "The local gateway did not accept this session. Restart it to try again.",
          gateway.log(),
        ),
      );
      break;
    default:
      break;
  }
});

ipcMain.on(CHANNEL.shellAction, (_event, action: ShellAction) => {
  if (action === "retry") {
    void relaunch();
  } else if (action === "configure") {
    void openConfiguration();
  }
});

ipcMain.handle(CHANNEL.configRead, () => config.read());
ipcMain.handle(
  CHANNEL.configSave,
  async (_event, text: string): Promise<ConfigSaveResult> => {
    const result = await config.save(text);
    if (result.ok) {
      // The gateway reads this file at startup and nowhere else, so a saved change
      // means nothing until it is restarted.
      void relaunch();
      return { ok: true };
    }
    return { ok: false, error: result.error };
  },
);

app.on("second-instance", () => {
  const existing = window;
  if (existing) {
    if (existing.isMinimized()) {
      existing.restore();
    }
    existing.focus();
  }
});

// One window, and closing it is quitting: there is nothing left to be running.
app.on("window-all-closed", () => app.quit());

app.on("before-quit", () => {
  // Belt and braces over the pipe: closing our end of the gateway's stdin is what
  // actually stops it, and the kernel does that for us however this process ends.
  void gateway.stop();
});

app
  .whenReady()
  .then(async () => {
    app.setAboutPanelOptions({
      applicationName: viewer.state.branding,
      applicationVersion: __VIEWER_VERSION__,
    });
    refuseEveryPermission();
    serveShellScheme({ web: layout.webRoot, shell: layout.shellRoot });
    getWindow();
    refresh();
    await launch();
  })
  // `launch` reports what the gateway does; this catches what nothing else can —
  // a page that would not load, which is every step of the sequence above. Without
  // it that is an unhandled rejection and a window showing nothing at all.
  .catch(report);
