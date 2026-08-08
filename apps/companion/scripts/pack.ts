// Zip `dist/` into the file a release carries.
//
// The zip is transport and nothing else. Chrome cannot load one — `Load unpacked`
// takes a directory — so what a user does with it is unzip it somewhere permanent and
// point Chrome at that. See docs/companion-extension.md.

import { mkdir, rm } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "./build.ts";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");

if (import.meta.main) {
  const version = await build();
  const out = join(root, "dist-crx");
  await rm(out, { recursive: true, force: true });
  await mkdir(out, { recursive: true });

  const archive = join(out, `remotex-companion-${version}.zip`);
  // `-r` from inside `dist`, so the archive's entries are `manifest.json` and friends
  // at the top rather than under a `dist/` directory nobody wants to unzip into.
  const zip = Bun.spawnSync(["zip", "-q", "-r", archive, "."], {
    cwd: join(root, "dist"),
  });
  if (zip.exitCode !== 0) {
    throw new Error(`zip failed: ${zip.stderr.toString()}`);
  }
  console.warn(`remotex-companion ${version} packed into ${archive}`);
}
