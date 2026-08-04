// Where the two things this bundle carries actually are.
//
// A packaged app has them beside it under `Contents/Resources`; a development run
// has them where the repo builds them. Environment variables rather than command
// line flags, so `--instance-dir` stays the only argument a GUI launch may carry —
// LaunchServices passes none of them anyway, and a flag that only works from a
// terminal is a flag that reads as broken from the Dock.

import { basename, join, resolve } from "node:path";

/**
 * Where the bundled main process, its preloads and the shell's pages ended up.
 *
 * Derived from Electron's `getAppPath` rather than from `__dirname`, and that is not
 * a style preference: bun resolves `__dirname` **at bundle time**, to the directory
 * of the source file. The bundle then looks for its preload beside `src/main/`,
 * finds nothing, and the page loses the two globals that tell it which host it is in
 * — so the client concludes it is in a browser and draws a login screen for a
 * gateway whose address it was never given. A window in which nothing works and
 * nothing looks broken.
 */
export function distDirFor(appPath: string): string {
  // Packaged, `getAppPath` is the asar and the bundle is `dist/` inside it; run from
  // disk as `electron dist/main.cjs`, it is that directory already.
  return basename(appPath) === "dist" ? appPath : join(appPath, "dist");
}

export interface ResourceLayout {
  /** The gateway binary, `Contents/Resources/remotex-gateway` when packaged. */
  binary: string;
  /** The SPA, `Contents/Resources/web` when packaged. */
  webRoot: string;
  /** The shell's own documents — the launch screen and the config editor. */
  shellRoot: string;
}

/**
 * Resolve the layout for this run.
 *
 * `appRoot` is `process.resourcesPath` when packaged and the repository root when
 * not; `dist` is where the bundled main process and its shell pages ended up.
 *
 * The development fallback is the **release** gateway, deliberately: an unpackaged
 * run is a manual QA run, and a debug build of the encoders is too slow to judge a
 * remote desktop by. Nothing in CI takes this path — the release job packages the
 * binary the tarball already published, and the shell's own tests run against the
 * fakes in `tests/fakes/` — so this costs a `cargo build --release` only where the
 * speed is the point. `REMOTEX_GATEWAY_BIN` is the way to point at another one.
 */
export function resourceLayout(
  packaged: boolean,
  appRoot: string,
  dist: string,
  env: NodeJS.ProcessEnv = process.env,
): ResourceLayout {
  return {
    binary:
      env.REMOTEX_GATEWAY_BIN ??
      (packaged
        ? join(appRoot, "remotex-gateway")
        : resolve(appRoot, "target/release/remotex")),
    webRoot:
      env.REMOTEX_WEB_ROOT ??
      (packaged ? join(appRoot, "web") : resolve(appRoot, "frontend/dist")),
    shellRoot: join(dist, "shell"),
  };
}
