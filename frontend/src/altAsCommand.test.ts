// Run with `bun test src/altAsCommand.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import { altAsCommand } from "./altAsCommand.ts";

test("the left Alt key is sent as the left Command key", () => {
  assert.equal(altAsCommand("AltLeft"), "MetaLeft");
});

test("the right Alt key is left alone, so Option stays reachable", () => {
  assert.equal(altAsCommand("AltRight"), "AltRight");
});

test("both Windows keys are sent as the right Command key", () => {
  assert.equal(altAsCommand("MetaLeft"), "MetaRight");
  assert.equal(altAsCommand("MetaRight"), "MetaRight");
});

test("the left Alt key shares its code with neither Windows key", () => {
  // Two keys on one code lift each other: releasing one sends the release the
  // remote applies to both. The pair that shares is the two Windows keys, which
  // is the pair a hand is least likely to hold at once.
  const command = ["MetaLeft", "MetaRight"].map(altAsCommand);
  assert.equal(command.includes(altAsCommand("AltLeft")), false);
});

test("everything else goes out as itself", () => {
  for (const code of ["KeyC", "ControlLeft", "ShiftRight", "CapsLock"]) {
    assert.equal(altAsCommand(code), code);
  }
});
