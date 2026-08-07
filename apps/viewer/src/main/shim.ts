// Instance apps: Chrome's app-shim convention, borrowed whole.
//
// Chrome puts each web app in `~/Applications/Chrome Apps.localized/<Name>.app` — a
// tiny bundle whose Info.plist names the profile (`CrAppModeUserDataDir`), the real
// browser (`CrBundlePath`) and, crucially, its own `CFBundleIdentifier`. That last
// key is the whole trick: macOS tells running apps apart by the bundle the running
// executable sits in, so a shim with its own identifier is its own Dock icon, its
// own name and its own ⌘Tab entry, no matter that everything it runs belongs to the
// browser beside it.
//
// The same shape here, with `--instance-dir` as the thing being pinned down:
//
//   ~/Applications/remotex Apps.localized/<Name>.app/Contents/
//     Info.plist        the main app's own plist, re-identified — carrying
//                       `RemotexInstanceDir` the way Chrome carries its profile key
//     MacOS/remotex     the main bundle's stub executable, cloned
//     Frameworks        the main bundle's frameworks and helpers, cloned
//     Resources/        its own icon, a cloned app.asar, and symlinks to web
//                       and the gateway
//
// The executable is the shim's own file, not a symlink, and not a wrapper that
// would exec the real one — either of those hands the process back to
// `/Applications/remotex.app` and with it the Dock icon this exists to keep.
// Electron's stub is a few tens of kilobytes that does nothing but load
// `../Frameworks`, which is exactly the role Chrome built `app_mode_loader` to
// play; copying it *is* the custom binary, already written.
//
// Frameworks are *cloned*, and that is a measurement, not a preference: with
// `Contents/Frameworks` (or any framework or helper inside it) symlinked into the
// main bundle, the browser process dies at launch on a wordless SIGTRAP deep in
// Chromium — same site every time, sandbox on or off. Clones are APFS `clonefile`
// copies (`COPYFILE_FICLONE`), so the 280 MB of frameworks share their blocks with
// the main app and the shim stays as cheap as Chrome's. `app.asar` is cloned with
// them: the stub, the frameworks and the JS are one versioned unit, and the shim's
// own plist seals that exact asar's hash under `ElectronAsarIntegrity`. What stays
// a symlink is the product — `web` and `remotex-gateway` — so a shim keeps serving
// the current client and gateway across upgrades of the app beside it, and
// recreating the app from the same folder refreshes the cloned shell in place.
//
// The plist is derived from the main app's rather than written fresh, for the same
// reason Chrome derives its shims: the keys that make the bundle *runnable* —
// `ElectronAsarIntegrity` above all — are the packager's, and a second copy of
// them here would rot.

import { createHash } from "node:crypto";
import { constants, existsSync } from "node:fs";
import {
  chmod,
  copyFile,
  cp,
  mkdir,
  readFile,
  rename,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { join, resolve } from "node:path";

/** Chrome's `CrAppModeUserDataDir`, for the directory `--instance-dir` would name. */
export const INSTANCE_DIR_KEY = "RemotexInstanceDir";
/** Chrome's `CrBundlePath`: where the bundle everything is borrowed from lives. */
export const MAIN_BUNDLE_KEY = "RemotexMainBundlePath";

// --- Just enough plist ------------------------------------------------------
//
// Not a plist parser — a pair of inverse functions over the five entities XML has,
// applied to `<key>K</key><string>V</string>` pairs whose keys this module chose.
// None of those keys recurs inside the plist's nested dictionaries, which is what
// makes exact-match string surgery sound here and nowhere else.

function escapeXml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}

function unescapeXml(value: string): string {
  return value
    .replaceAll("&lt;", "<")
    .replaceAll("&gt;", ">")
    .replaceAll("&quot;", '"')
    .replaceAll("&apos;", "'")
    .replaceAll("&amp;", "&");
}

/** The string under `key`, or null when the plist does not carry it. */
export function readPlistString(plist: string, key: string): string | null {
  const match = plist.match(
    new RegExp(`<key>${key}</key>\\s*<string>([^<]*)</string>`),
  );
  return match ? unescapeXml(match[1]) : null;
}

/** Replace `key`'s string, or add the pair before the closing of the top dict. */
export function upsertPlistString(
  plist: string,
  key: string,
  value: string,
): string {
  const escaped = escapeXml(value);
  const existing = new RegExp(
    `(<key>${key}</key>\\s*<string>)[^<]*(</string>)`,
  );
  if (existing.test(plist)) {
    return plist.replace(existing, `$1${escaped}$2`);
  }
  // The top-level dict is the last one to close.
  const closing = plist.lastIndexOf("</dict>");
  if (closing === -1) {
    throw new Error("not a plist: no dict to add to");
  }
  return (
    plist.slice(0, closing) +
    `\t<key>${key}</key>\n\t<string>${escaped}</string>\n` +
    plist.slice(closing)
  );
}

// --- Identity ---------------------------------------------------------------

/**
 * `dev.remotex.viewer.instance.<hash of the instance directory>`.
 *
 * Hashed from the directory rather than the display name, because the directory
 * *is* the instance: the same folder always makes the same app — recreating it is
 * a refresh, and TCC grants and saved window state stay put — and two folders can
 * never collide into one Dock icon by sharing a basename.
 */
export function shimBundleId(mainId: string, instanceDir: string): string {
  const hash = createHash("sha256")
    .update(instanceDir)
    .digest("hex")
    .slice(0, 12);
  return `${mainId}.instance.${hash}`;
}

/**
 * A folder's basename, made bearable as an app name.
 *
 * `/` cannot appear in a basename, and `:` is the one character HFS swaps with it —
 * both become `-` rather than an error, because the folder was chosen first and its
 * name should not be able to veto the app.
 */
export function shimName(raw: string): string {
  const name = raw.replaceAll(/[/:]/g, "-").trim();
  if (name === "" || name.startsWith(".")) {
    throw new Error(`"${raw}" does not name an app`);
  }
  return name;
}

/**
 * The main app's plist, re-identified as one shim's.
 *
 * `CFBundleDisplayName` — what the Dock and ⌘Tab write under the icon — becomes the
 * shim's own name, but `CFBundleName` deliberately stays the main app's: Electron
 * derives the helper bundles' names from it (`GetHelperAppPath` wants
 * `<CFBundleName> Helper.app` inside `Contents/Frameworks`), so a shim renaming it
 * dies at launch, unable to find a "shim Helper.app" that never existed.
 */
export function shimPlist(
  mainPlist: string,
  spec: { name: string; instanceDir: string; mainBundle: string },
): string {
  const mainId = readPlistString(mainPlist, "CFBundleIdentifier");
  if (mainId === null) {
    throw new Error("the main app's Info.plist has no CFBundleIdentifier");
  }
  let plist = mainPlist;
  plist = upsertPlistString(
    plist,
    "CFBundleIdentifier",
    shimBundleId(mainId, spec.instanceDir),
  );
  plist = upsertPlistString(plist, "CFBundleDisplayName", spec.name);
  plist = upsertPlistString(plist, INSTANCE_DIR_KEY, spec.instanceDir);
  plist = upsertPlistString(plist, MAIN_BUNDLE_KEY, spec.mainBundle);
  return plist;
}

// --- What a shim launch reads back ------------------------------------------

/** A read that answers null for a file that is not there. */
export type PlistReader = (path: string) => string | null;

function ownPlist(execPath: string, read: PlistReader): string | null {
  // `<bundle>.app/Contents/MacOS/remotex` → `<bundle>.app/Contents/Info.plist`.
  return read(resolve(execPath, "..", "..", "Info.plist"));
}

/**
 * The instance directory this bundle was created to carry, if it is a shim.
 *
 * The main app's plist has no such key, so a double-clicked `remotex.app` answers
 * null here and keeps its default instance — which is what makes this safe to ask
 * unconditionally, between `--instance-dir` and that default.
 */
export function instanceDirFromBundle(
  execPath: string,
  read: PlistReader,
): string | null {
  const plist = ownPlist(execPath, read);
  return plist === null ? null : readPlistString(plist, INSTANCE_DIR_KEY);
}

/**
 * The bundle to borrow from when *this* launch creates a shim.
 *
 * From the main app that is its own bundle; from a shim it is the recorded
 * `RemotexMainBundlePath`, so a shim making a shim links to the real app rather
 * than chaining symlinks through itself.
 */
export function mainBundleFor(execPath: string, read: PlistReader): string {
  const own = resolve(execPath, "..", "..", "..");
  const plist = ownPlist(execPath, read);
  return plist === null
    ? own
    : (readPlistString(plist, MAIN_BUNDLE_KEY) ?? own);
}

// --- Creation ---------------------------------------------------------------

export interface ShimSpec {
  /** Absolute path of the instance directory the app will pin down. */
  instanceDir: string;
  /** What the Dock, Finder and ⌘Tab call it. Usually the folder's basename. */
  name: string;
  /** `~/Applications/remotex Apps.localized`, made on first use like Chrome's. */
  appsDir: string;
  /** The bundle everything is borrowed from: `/Applications/remotex.app`. */
  mainBundle: string;
  /** The `.icns` copied in as this app's own face. */
  icon: string;
}

/**
 * Write `<appsDir>/<Name>.app`, and answer where it landed.
 *
 * Recreating from the same instance directory replaces the bundle in place — that
 * is the refresh path, and it is also why anything *else* already wearing the name
 * is refused rather than replaced: a different instance's shim would be silently
 * retargeted, and a bundle without our key was never ours to delete.
 */
export async function createShimBundle(spec: ShimSpec): Promise<string> {
  const name = shimName(spec.name);
  const bundle = join(spec.appsDir, `${name}.app`);
  const instanceDir = resolve(spec.instanceDir);

  const existing = await readFile(
    join(bundle, "Contents", "Info.plist"),
    "utf8",
  )
    .then((text) => readPlistString(text, INSTANCE_DIR_KEY))
    .catch((error: NodeJS.ErrnoException) => {
      if (error.code === "ENOENT") {
        return null;
      }
      throw error;
    });
  if (existsSync(bundle) && existing === null) {
    throw new Error(`${bundle} exists and is not a remotex instance app.`);
  }
  if (existing !== null && existing !== instanceDir) {
    throw new Error(`${bundle} is already the app for ${existing}.`);
  }

  const mainContents = join(spec.mainBundle, "Contents");
  const mainPlist = await readFile(join(mainContents, "Info.plist"), "utf8");
  const plist = shimPlist(mainPlist, {
    name,
    instanceDir,
    mainBundle: spec.mainBundle,
  });

  // The `.localized` marker beside the shims is what makes Finder display the
  // folder as "remotex Apps" in the user's language — the same empty directory
  // Chrome leaves in its own.
  await mkdir(join(spec.appsDir, ".localized"), { recursive: true });

  // Staged whole and renamed into place, like the config store's writes: a launch
  // must never find half an app, least of all the refresh of one that works.
  const staging = `${bundle}.new-${process.pid}`;
  await rm(staging, { recursive: true, force: true });
  try {
    const contents = join(staging, "Contents");
    await mkdir(join(contents, "MacOS"), { recursive: true });
    await mkdir(join(contents, "Resources"), { recursive: true });
    await writeFile(join(contents, "Info.plist"), plist);
    await writeFile(join(contents, "PkgInfo"), "APPL????");
    // `FICLONE`, not `FICLONE_FORCE`: on APFS these are free block-sharing
    // clones, and anywhere else they quietly become plain copies.
    await copyFile(
      join(mainContents, "MacOS", "remotex"),
      join(contents, "MacOS", "remotex"),
      constants.COPYFILE_FICLONE,
    );
    await chmod(join(contents, "MacOS", "remotex"), 0o755);
    // `verbatimSymlinks` because a framework's insides are relative symlinks
    // (`Resources -> Versions/Current/Resources`), and resolving them while
    // copying would point pieces of the clone back at the main bundle — which is
    // the exact shape the clone exists to avoid.
    await cp(join(mainContents, "Frameworks"), join(contents, "Frameworks"), {
      recursive: true,
      verbatimSymlinks: true,
      mode: constants.COPYFILE_FICLONE,
    });
    const resources = join(mainContents, "Resources");
    await copyFile(
      join(resources, "app.asar"),
      join(contents, "Resources", "app.asar"),
      constants.COPYFILE_FICLONE,
    );
    for (const borrowed of ["web", "remotex-gateway"]) {
      await symlink(
        join(resources, borrowed),
        join(contents, "Resources", borrowed),
      );
    }
    // Its own file, not a link: the icon is the one resource that must *differ*
    // from the main app's, or the Dock shows two of the same face.
    await copyFile(spec.icon, join(contents, "Resources", "icon.icns"));
    await rm(bundle, { recursive: true, force: true });
    await rename(staging, bundle);
  } catch (error) {
    await rm(staging, { recursive: true, force: true });
    throw error;
  }
  return bundle;
}
