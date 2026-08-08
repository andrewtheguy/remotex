// What the "This session" card says about the connection.
//
// The case that matters is the one where `protocol` alone is not an answer: three
// targets say `vnc`, and what a person notices about them — a display list, a
// target that follows its window or keeps a fixed display, a path that is reverse
// engineered — differs by subtype and by nothing else on the wire.

import assert from "node:assert/strict";
import { test } from "node:test";

import { connectionLabel, connectionShortLabel } from "./connectionLabel.ts";

test("a target with no subtype is just its protocol", () => {
  assert.equal(connectionLabel("rdp", null), "RDP");
  assert.equal(connectionLabel("vnc", null), "VNC");
});

test("the two Apple modes say which one they are, in the config's own spelling", () => {
  // The spelling is kept so the line can be found in `remotex.toml`, and the
  // description because "ard" says nothing to anybody who did not write it.
  assert.equal(
    connectionLabel("vnc", "ard"),
    "VNC · Apple Screen Sharing, Standard mode (ard)",
  );
  // Experimental is part of the name here on purpose: it is the one path built
  // with no specification behind it, and this card is where somebody looks when it
  // has behaved oddly.
  assert.equal(
    connectionLabel("vnc", "ard-high-performance"),
    "VNC · Apple Screen Sharing, High Performance — experimental (ard-high-performance)",
  );
});

test("the picker's row keeps the spelling and drops the prose", () => {
  // That row also carries a host and a port, and it is read while choosing rather
  // than while diagnosing: the config key is the whole of what distinguishes two
  // Macs in the list, and the sentence would wrap.
  assert.equal(connectionShortLabel("rdp", null), "RDP");
  assert.equal(connectionShortLabel("vnc", "ard"), "VNC · ard");
  assert.equal(
    connectionShortLabel("vnc", "ard-high-performance"),
    "VNC · ard-high-performance",
  );
});

test("a subtype this build has never heard of still names itself", () => {
  // The gateway and this client ship together, so this is a build mismatch rather
  // than a new feature — and a row that silently dropped the value would describe
  // the session wrongly rather than incompletely.
  assert.equal(
    connectionLabel("vnc", "ard-something-new"),
    "VNC · ard-something-new",
  );
});
