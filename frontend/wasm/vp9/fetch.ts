// The page's software VP9 decoder (andrewtheguy/vp9-wasm), which is not built
// here: `pkg/` beside this file is the release `pin.json` names by version and
// SHA-256, unpacked. The frontend's `build:wasm` runs this before anything
// reads the module, and it does nothing while `pkg/` holds that release.
//
// The archive is the one at `REMOTEX_VP9_WASM_ARCHIVE` where that is set, and
// the release's own download otherwise. Either is refused unless it is exactly
// the pinned release.

import { existsSync, mkdirSync, rmSync } from "node:fs";
import { join } from "node:path";
import pin from "./pin.json" with { type: "json" };

/** What a release holds, as wasm-pack names a module: glue, module, types. */
const FILES = ["vp9.js", "vp9_bg.wasm", "vp9.d.ts"];

const name = `vp9-wasm-v${pin.version}.tar.gz`;
const url = `https://github.com/andrewtheguy/vp9-wasm/releases/download/v${pin.version}/${name}`;
const pkg = join(import.meta.dir, "pkg");
/** The digest of the archive `pkg/` was unpacked from. */
const stamp = join(pkg, ".sha256");
/** How long the release's download may take, headers and body. */
const DOWNLOAD_TIMEOUT_MS = 60_000;

async function unpacked(): Promise<boolean> {
  return (
    FILES.every((file) => existsSync(join(pkg, file))) &&
    existsSync(stamp) &&
    (await Bun.file(stamp).text()).trim() === pin.sha256
  );
}

async function archive(): Promise<{ bytes: Uint8Array; from: string }> {
  const local = process.env.REMOTEX_VP9_WASM_ARCHIVE;
  if (local) {
    return { bytes: await Bun.file(local).bytes(), from: local };
  }
  const instead = `set REMOTEX_VP9_WASM_ARCHIVE to a copy of ${name}`;
  try {
    // The headers and the body both: a download that stalls ends the build
    // with what to do about it, not a hang.
    const response = await fetch(url, {
      signal: AbortSignal.timeout(DOWNLOAD_TIMEOUT_MS),
    });
    if (!response.ok) {
      throw new Error(`${url} answered ${response.status}: ${instead}`);
    }
    return { bytes: await response.bytes(), from: url };
  } catch (cause) {
    if (cause instanceof Error && cause.message.endsWith(instead)) {
      throw cause;
    }
    const timedOut = cause instanceof Error && cause.name === "TimeoutError";
    throw new Error(
      `${url} ${timedOut ? `did not arrive in ${DOWNLOAD_TIMEOUT_MS / 1000} s` : "could not be fetched"}: ${instead}`,
      { cause },
    );
  }
}

/** The regular files of a tar archive, by name. */
function entries(tar: Uint8Array): Map<string, Uint8Array> {
  const text = new TextDecoder();
  const field = (at: number, length: number) =>
    text.decode(tar.subarray(at, at + length)).replace(/\0.*$/s, "");
  const files = new Map<string, Uint8Array>();
  for (let at = 0; at + 512 <= tar.length; ) {
    const name = field(at, 100);
    if (name === "") {
      break;
    }
    const size = Number.parseInt(field(at + 124, 12).trim(), 8);
    const type = field(at + 156, 1);
    if (type === "0" || type === "") {
      files.set(name, tar.subarray(at + 512, at + 512 + size));
    }
    at += 512 + Math.ceil(size / 512) * 512;
  }
  return files;
}

if (!(await unpacked())) {
  const { bytes, from } = await archive();
  const sha256 = new Bun.CryptoHasher("sha256").update(bytes).digest("hex");
  if (sha256 !== pin.sha256) {
    throw new Error(
      `${from} is not vp9-wasm v${pin.version}: its SHA-256 is ${sha256}, and wasm/vp9/pin.json pins ${pin.sha256}`,
    );
  }
  const held = entries(Bun.gunzipSync(bytes));
  rmSync(pkg, { recursive: true, force: true });
  mkdirSync(pkg);
  for (const file of FILES) {
    const content = held.get(file);
    if (!content) {
      throw new Error(`${from} holds no ${file}`);
    }
    await Bun.write(join(pkg, file), content);
  }
  // Last: a `pkg/` without it is unpacked again.
  await Bun.write(stamp, `${pin.sha256}\n`);
  process.stdout.write(`vp9-wasm v${pin.version} unpacked from ${from}\n`);
}
