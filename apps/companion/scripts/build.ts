// Bundle the extension: one service worker, one content script, one offscreen
// document, one popup.
//
// The content script is the only one that is not an ES module, and that is not a
// preference: Chrome loads a `content_scripts` file as a classic script, so the bundle
// must have no imports left in it.

import { cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { chromeVersion } from "../src/shared/version.ts";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const dist = join(root, "dist");

/**
 * The workspace version, read out of `Cargo.toml`.
 *
 * The same regex `apps/viewer/scripts/build.ts` uses, and the same reason: one version
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
  const cargo = await cargoVersion();
  await rm(dist, { recursive: true, force: true });
  await mkdir(dist, { recursive: true });

  // One build per module, because all three entry points are called `main.ts` and
  // `[name].js` would have them overwrite each other. Named for the context each one
  // runs in, which is what the manifest and the two HTML files ask for.
  for (const context of ["worker", "offscreen", "popup"]) {
    const built = await Bun.build({
      entrypoints: [join(root, `src/${context}/main.ts`)],
      outdir: dist,
      target: "browser",
      format: "esm",
      naming: `${context}.js`,
    });
    throwOnFailure(built, context);
  }

  const content = await Bun.build({
    entrypoints: [join(root, "src/content/main.ts")],
    outdir: dist,
    target: "browser",
    format: "iife",
    naming: "content.js",
  });
  throwOnFailure(content, "content");

  await writeFile(
    join(dist, "manifest.json"),
    `${JSON.stringify(await manifest(cargo), null, 2)}\n`,
  );
  await cp(
    join(root, "src/offscreen/offscreen.html"),
    join(dist, "offscreen.html"),
  );
  await cp(join(root, "src/popup/popup.html"), join(dist, "popup.html"));
  await cp(join(root, "src/popup/popup.css"), join(dist, "popup.css"));
  await cp(join(root, "icons"), join(dist, "icons"), { recursive: true });
  return cargo;
}

/**
 * The manifest as it ships: the repository's copy with the version put in.
 *
 * The two version fields are the only difference. Everything else — and in particular
 * the permission arrays — is what `tests/manifest.test.ts` reads, so there is one
 * description of what this extension may do rather than a source one and a built one.
 */
async function manifest(cargo: string): Promise<Record<string, unknown>> {
  const source = JSON.parse(
    await readFile(join(root, "src/manifest.json"), "utf8"),
  ) as Record<string, unknown>;
  return { ...source, ...chromeVersion(cargo) };
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
  console.warn(`remotex-companion ${version} built into dist/`);
}
