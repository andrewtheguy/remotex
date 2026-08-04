// Where the two things this bundle carries actually are.
//
// A packaged app has them beside it under `Contents/Resources`; a development run
// has them where the repo builds them. Environment variables rather than command
// line flags, so `--instance-dir` stays the only argument a GUI launch may carry —
// LaunchServices passes none of them anyway, and a flag that only works from a
// terminal is a flag that reads as broken from the Dock.

import { join, resolve } from "node:path";

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
