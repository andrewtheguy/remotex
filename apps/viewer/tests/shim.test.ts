// Instance apps: the Chrome-style shim bundle, from the plist surgery up.
//
// The bundles here are fakes built in a temp directory — a few files standing where
// `/Applications/remotex.app` would — because what is under test is the *shape* the
// creator writes and the rules it refuses by, not Electron's ability to run one.

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readlinkSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  createShimBundle,
  INSTANCE_DIR_KEY,
  instanceDirFromBundle,
  MAIN_BUNDLE_KEY,
  mainBundleFor,
  readPlistString,
  shimBundleId,
  shimName,
  upsertPlistString,
} from "../src/main/shim.ts";

// A trimmed copy of what electron-builder writes: the identity keys, and the one
// nested dict whose survival the whole derive-don't-rewrite design exists for.
const MAIN_PLIST = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
\t<key>CFBundleDisplayName</key>
\t<string>remotex</string>
\t<key>CFBundleExecutable</key>
\t<string>remotex</string>
\t<key>CFBundleIdentifier</key>
\t<string>dev.remotex.viewer</string>
\t<key>CFBundleName</key>
\t<string>remotex</string>
\t<key>ElectronAsarIntegrity</key>
\t<dict>
\t\t<key>Resources/app.asar</key>
\t\t<dict>
\t\t\t<key>algorithm</key>
\t\t\t<string>SHA256</string>
\t\t\t<key>hash</key>
\t\t\t<string>30e672a7232b845235c40cc90d1b2474e5d3caccf946abb044901752bdf565ac</string>
\t\t</dict>
\t</dict>
\t<key>LSMinimumSystemVersion</key>
\t<string>13.0</string>
</dict>
</plist>`;

let work: string;

beforeEach(() => {
  work = mkdtempSync(join(tmpdir(), "remotex-shim-"));
});

afterEach(() => {
  rmSync(work, { recursive: true, force: true });
});

/** A stand-in for the installed bundle: the files the creator clones or links. */
function fakeMainBundle(): string {
  const bundle = join(work, "remotex.app");
  const contents = join(bundle, "Contents");
  mkdirSync(join(contents, "MacOS"), { recursive: true });
  mkdirSync(join(contents, "Resources", "web"), { recursive: true });
  // A framework the way macOS lays one out: content under Versions, reached
  // through a relative symlink at the framework root.
  const framework = join(contents, "Frameworks", "Fake.framework");
  mkdirSync(join(framework, "Versions", "A"), { recursive: true });
  writeFileSync(join(framework, "Versions", "A", "Fake"), "the framework");
  symlinkSync(join("Versions", "A"), join(framework, "Current"));
  writeFileSync(join(contents, "Info.plist"), MAIN_PLIST);
  writeFileSync(join(contents, "MacOS", "remotex"), "the stub", {
    mode: 0o755,
  });
  writeFileSync(join(contents, "Resources", "app.asar"), "asar");
  writeFileSync(join(contents, "Resources", "remotex-gateway"), "gateway");
  writeFileSync(join(contents, "Resources", "instance-icon.icns"), "amber");
  return bundle;
}

const readOrNull = (path: string): string | null => {
  try {
    return readFileSync(path, "utf8");
  } catch {
    return null;
  }
};

describe("just enough plist", () => {
  test("a present key is read back, an absent one is null", () => {
    expect(readPlistString(MAIN_PLIST, "CFBundleIdentifier")).toBe(
      "dev.remotex.viewer",
    );
    expect(readPlistString(MAIN_PLIST, INSTANCE_DIR_KEY)).toBeNull();
  });

  test("upsert replaces in place and inserts into the top dict, not a nested one", () => {
    const replaced = upsertPlistString(MAIN_PLIST, "CFBundleName", "work");
    expect(readPlistString(replaced, "CFBundleName")).toBe("work");
    // Everything else stands, the asar seal above all.
    expect(replaced).toContain("30e672a7232b845235c40cc90d1b2474e5d3caccf946");

    const added = upsertPlistString(MAIN_PLIST, INSTANCE_DIR_KEY, "/tmp/qa");
    expect(readPlistString(added, INSTANCE_DIR_KEY)).toBe("/tmp/qa");
    // Inserted before the *last* </dict> — inside the top dict, after the nested
    // ElectronAsarIntegrity one has already closed.
    expect(added.indexOf("</plist>")).toBeGreaterThan(
      added.indexOf(INSTANCE_DIR_KEY),
    );
    expect(added.indexOf(INSTANCE_DIR_KEY)).toBeGreaterThan(
      added.indexOf("ElectronAsarIntegrity"),
    );
  });

  test("a value round-trips through XML, however the folder is named", () => {
    const dir = "/Users/a/QA & <trials>/one";
    const written = upsertPlistString(MAIN_PLIST, INSTANCE_DIR_KEY, dir);
    expect(written).not.toContain("QA & <");
    expect(readPlistString(written, INSTANCE_DIR_KEY)).toBe(dir);
  });
});

describe("identity", () => {
  test("the bundle id comes from the directory, so the same folder is the same app", () => {
    const one = shimBundleId("dev.remotex.viewer", "/instances/work");
    expect(one).toStartWith("dev.remotex.viewer.instance.");
    expect(shimBundleId("dev.remotex.viewer", "/instances/work")).toBe(one);
    expect(shimBundleId("dev.remotex.viewer", "/instances/home")).not.toBe(one);
  });

  test("a folder's name is made bearable rather than allowed to veto the app", () => {
    expect(shimName("work: rdp/vnc")).toBe("work- rdp-vnc");
    expect(() => shimName("  ")).toThrow();
    expect(() => shimName(".hidden")).toThrow();
  });
});

describe("what a launch reads back", () => {
  test("a shim's plist names its instance; the main app's answers null", () => {
    const bundle = fakeMainBundle();
    const exec = join(bundle, "Contents", "MacOS", "remotex");
    expect(instanceDirFromBundle(exec, readOrNull)).toBeNull();
    expect(mainBundleFor(exec, readOrNull)).toBe(bundle);
  });

  test("a shim making a shim borrows from the recorded main bundle, not itself", async () => {
    const main = fakeMainBundle();
    const shim = await createShimBundle({
      instanceDir: join(work, "instances", "work"),
      name: "work",
      appsDir: join(work, "Apps.localized"),
      mainBundle: main,
      icon: join(main, "Contents", "Resources", "instance-icon.icns"),
    });
    const exec = join(shim, "Contents", "MacOS", "remotex");
    expect(instanceDirFromBundle(exec, readOrNull)).toBe(
      join(work, "instances", "work"),
    );
    expect(mainBundleFor(exec, readOrNull)).toBe(main);
  });
});

describe("creating the bundle", () => {
  const create = (name: string, dir?: string) => {
    const main = join(work, "remotex.app");
    return createShimBundle({
      instanceDir: dir ?? join(work, "instances", name),
      name,
      appsDir: join(work, "Apps.localized"),
      mainBundle: main,
      icon: join(main, "Contents", "Resources", "instance-icon.icns"),
    });
  };

  test("the shape is Chrome's: own identity and icon, cloned shell, linked product", async () => {
    const main = fakeMainBundle();
    const bundle = await create("work");
    expect(bundle).toBe(join(work, "Apps.localized", "work.app"));
    const contents = join(bundle, "Contents");

    // The executable is the shim's own file — the running process must sit in
    // this bundle, because the bundle the process sits in is what owns the Dock
    // icon.
    expect(lstatSync(join(contents, "MacOS", "remotex")).isSymbolicLink()).toBe(
      false,
    );
    expect(readFileSync(join(contents, "MacOS", "remotex"), "utf8")).toBe(
      "the stub",
    );
    expect(statSync(join(contents, "MacOS", "remotex")).mode & 0o755).toBe(
      0o755,
    );

    // Frameworks and asar are clones, not symlinks: a symlinked framework path
    // is the shape Chromium dies on at launch. The framework's *internal*
    // relative symlink must survive verbatim, still relative, still inside the
    // clone.
    expect(lstatSync(join(contents, "Frameworks")).isSymbolicLink()).toBe(
      false,
    );
    expect(
      readFileSync(
        join(contents, "Frameworks", "Fake.framework", "Versions", "A", "Fake"),
        "utf8",
      ),
    ).toBe("the framework");
    expect(
      readlinkSync(join(contents, "Frameworks", "Fake.framework", "Current")),
    ).toBe(join("Versions", "A"));
    expect(
      lstatSync(join(contents, "Resources", "app.asar")).isSymbolicLink(),
    ).toBe(false);
    expect(readFileSync(join(contents, "Resources", "app.asar"), "utf8")).toBe(
      "asar",
    );

    // The product stays the main app's, so the shim tracks its upgrades.
    for (const borrowed of ["web", "remotex-gateway"]) {
      expect(readlinkSync(join(contents, "Resources", borrowed))).toBe(
        join(main, "Contents", "Resources", borrowed),
      );
    }
    // Its own icon file, not a link: differing from the main app's is its job.
    expect(
      lstatSync(join(contents, "Resources", "icon.icns")).isSymbolicLink(),
    ).toBe(false);
    expect(readFileSync(join(contents, "Resources", "icon.icns"), "utf8")).toBe(
      "amber",
    );
    expect(readFileSync(join(contents, "PkgInfo"), "utf8")).toBe("APPL????");
    // The marker that makes Finder localize "remotex Apps", as Chrome's does.
    expect(
      statSync(join(work, "Apps.localized", ".localized")).isDirectory(),
    ).toBe(true);
  });

  test("the plist is the main app's, re-identified", async () => {
    fakeMainBundle();
    const bundle = await create("work");
    const plist = readFileSync(join(bundle, "Contents", "Info.plist"), "utf8");
    expect(readPlistString(plist, "CFBundleIdentifier")).toBe(
      shimBundleId("dev.remotex.viewer", join(work, "instances", "work")),
    );
    expect(readPlistString(plist, "CFBundleDisplayName")).toBe("work");
    // Not renamed: Electron finds its helper bundles by `<CFBundleName> Helper.app`,
    // so this key changing is the difference between a Dock icon and a crash.
    expect(readPlistString(plist, "CFBundleName")).toBe("remotex");
    expect(readPlistString(plist, INSTANCE_DIR_KEY)).toBe(
      join(work, "instances", "work"),
    );
    expect(readPlistString(plist, MAIN_BUNDLE_KEY)).toBe(
      join(work, "remotex.app"),
    );
    // Derived, not rewritten: the packager's keys ride along, and the asar seal is
    // the one an integrity-checking Electron would refuse the symlinked asar
    // without.
    expect(plist).toContain("ElectronAsarIntegrity");
    expect(readPlistString(plist, "CFBundleExecutable")).toBe("remotex");
  });

  test("the same folder recreates in place; anything else wearing the name is refused", async () => {
    fakeMainBundle();
    await create("work");
    // Same instance directory: a refresh, not a conflict.
    await create("work");

    // Same name, different instance: refusing is the only honest answer.
    await expect(
      create("work", join(work, "instances", "other")),
    ).rejects.toThrow(/already the app for/);

    // A bundle that was never ours is never ours to delete.
    mkdirSync(join(work, "Apps.localized", "mine.app"), { recursive: true });
    await expect(create("mine")).rejects.toThrow(/not a remotex instance app/);
  });
});
