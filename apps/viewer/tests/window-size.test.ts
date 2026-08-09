// What the window-size memory accepts, refuses, and clamps.
//
// The floor under test is the *remembered* one: the live window may legally be
// smaller (MINIMUM_SIZE in window.ts), so both directions — writing a size down
// and reading one back — must raise to 800×600 rather than trust the caller or
// the file.

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  clampRemembered,
  REMEMBERED_MINIMUM,
  rememberedSizeFrom,
  WindowSizeStore,
} from "../src/main/window-size.ts";

let dir: string;
let path: string;

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "remotex-window-size-"));
  path = join(dir, "profile", "window-size.json");
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

describe("parsing", () => {
  test("a written size reads back as itself", () => {
    const store = new WindowSizeStore(path);
    store.remember({ width: 1440, height: 900 });
    expect(store.read()).toEqual({ width: 1440, height: 900 });
  });

  test("no file is no memory", () => {
    expect(new WindowSizeStore(path).read()).toBeNull();
  });

  test("a torn or hand-mangled file is no memory, not a NaN window", () => {
    for (const text of [
      "",
      "{",
      "null",
      "[800,600]",
      '{"width":800}',
      '{"width":"800","height":"600"}',
      '{"width":NaN,"height":600}',
      '{"width":0,"height":600}',
      '{"width":-1280,"height":800}',
    ]) {
      expect(rememberedSizeFrom(text)).toBeNull();
    }
  });

  test("Infinity is a number and still refused", () => {
    expect(rememberedSizeFrom('{"width":1e999,"height":800}')).toBeNull();
  });
});

describe("the remembered floor", () => {
  test("a size below 800×600 is written down as 800×600", () => {
    // The live window may be squeezed to MINIMUM_SIZE, but a session that ended
    // in a sliver must not start the next one in one.
    const store = new WindowSizeStore(path);
    store.remember({ width: 720, height: 480 });
    expect(store.read()).toEqual(REMEMBERED_MINIMUM);
    expect(JSON.parse(readFileSync(path, "utf8"))).toEqual(REMEMBERED_MINIMUM);
  });

  test("the floor holds per axis, not as a pair", () => {
    expect(clampRemembered({ width: 720, height: 1000 })).toEqual({
      width: 800,
      height: 1000,
    });
    expect(clampRemembered({ width: 1600, height: 480 })).toEqual({
      width: 1600,
      height: 600,
    });
  });

  test("a file edited below the floor reads back at the floor", () => {
    writeFileSync(join(dir, "edited.json"), '{"width":100,"height":100}');
    expect(new WindowSizeStore(join(dir, "edited.json")).read()).toEqual(
      REMEMBERED_MINIMUM,
    );
  });

  test("fractional sizes come back as whole pixels", () => {
    expect(clampRemembered({ width: 1279.6, height: 800.4 })).toEqual({
      width: 1280,
      height: 800,
    });
  });
});
