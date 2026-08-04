// Build the shell, then assemble remotex.app and its disk image.
//
// Two things this decides that `electron-builder.yml` cannot: the version, which
// comes from `Cargo.toml` and not from a `package.json` pretending to be a second
// source of truth, and whether the image says it is unsigned.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "./build.ts";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const repo = join(root, "../..");

const version = await build();

// Named rather than found: an app missing either of them is not an app, and the
// error should say which half, not "no such file" from inside electron-builder.
for (const [what, path] of [
  ["the gateway", join(repo, "target/release/remotex")],
  ["the client", join(repo, "frontend/dist/index.html")],
] as const) {
  if (!existsSync(path)) {
    console.error(
      `remotex: ${what} is not built (${path}).\n` +
        "  cargo build --release            # the gateway\n" +
        "  (cd frontend && bun run build)   # the client",
    );
    process.exit(1);
  }
}

const identity = process.env.CODESIGN_IDENTITY;
const result = spawnSync(
  join(root, "node_modules/.bin/electron-builder"),
  [
    "--mac",
    "--config",
    join(root, "electron-builder.yml"),
    `--config.extraMetadata.version=${version}`,
    // The checked-in package.json stays 0.0.0; this is what reaches
    // CFBundleShortVersionString, CFBundleVersion and the image's name.
    ...(identity ? [`--config.mac.identity=${identity}`] : []),
  ],
  {
    cwd: root,
    stdio: "inherit",
    env: {
      ...process.env,
      // The two names the branch shipped, kept so the release job's checks and
      // anyone's muscle memory both still work.
      REMOTEX_DMG_SUFFIX: identity ? "" : "-unsigned",
    },
  },
);

process.exit(result.status ?? 1);
