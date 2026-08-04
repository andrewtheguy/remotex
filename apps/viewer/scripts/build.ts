// Bundle the shell: one main process, two preloads, two documents.
//
// CommonJS for the first three, and not a preference: a sandboxed preload is loaded
// as a single CommonJS file, and Electron's own entry point is `require`d. The two
// shell documents are ordinary modules in a browser, because that is what they are.

import { cp, mkdir, readFile, rm } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const dist = join(root, "dist");

/**
 * The workspace version, read out of `Cargo.toml`.
 *
 * The same regex `frontend/vite.config.ts` uses, and the same reason: one version
 * for the whole repo, in one file, and no `package.json` pretending to be a second
 * source of truth.
 */
export async function cargoVersion(): Promise<string> {
  const manifest = await readFile(join(root, "../../Cargo.toml"), "utf8");
  const match = manifest.match(/^version = "(.+?)"/m);
  if (!match) {
    throw new Error("no version in Cargo.toml");
  }
  return match[1];
}

export async function build(): Promise<string> {
  const version = await cargoVersion();
  await rm(dist, { recursive: true, force: true });
  await mkdir(dist, { recursive: true });

  const main = await Bun.build({
    entrypoints: [join(root, "src/main/main.ts")],
    outdir: dist,
    target: "node",
    format: "cjs",
    external: ["electron"],
    define: { __VIEWER_VERSION__: JSON.stringify(version) },
    naming: "[dir]/main.cjs",
  });
  throwOnFailure(main, "main");

  const preloads = await Bun.build({
    entrypoints: [
      join(root, "src/preload/client.ts"),
      join(root, "src/preload/shell.ts"),
    ],
    outdir: dist,
    target: "node",
    format: "cjs",
    external: ["electron"],
    naming: "[dir]/[name].cjs",
  });
  throwOnFailure(preloads, "preload");

  const pages = await Bun.build({
    entrypoints: [
      join(root, "src/shell/launch.ts"),
      join(root, "src/shell/config.ts"),
    ],
    outdir: join(dist, "shell"),
    target: "browser",
    format: "esm",
  });
  throwOnFailure(pages, "shell");

  for (const asset of ["launch.html", "config.html", "shell.css"]) {
    await cp(join(root, "src/shell", asset), join(dist, "shell", asset));
  }
  return version;
}

function throwOnFailure(
  result: { success: boolean; logs: unknown[] },
  what: string,
) {
  if (!result.success) {
    for (const entry of result.logs) {
      console.error(entry);
    }
    throw new Error(`${what} did not build`);
  }
}

if (import.meta.main) {
  const version = await build();
  console.warn(`remotex-viewer ${version} built into dist/`);
}
