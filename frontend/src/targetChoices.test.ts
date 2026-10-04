// Which options a target shows at the picker, which are greyed, and what Start
// then sends. The properties under test are the picker's rules: the size a session
// will have is always shown and is a choice only where there are two, not offered
// is not shown, offered but unavailable is greyed with the reason, and a target
// that can only be passed cannot start in a browser that cannot take it.
import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";
import {
  type Abilities,
  readRememberedChoices,
  rememberChoice,
  type TargetInfo,
  targetOptions,
} from "./targetChoices.ts";

// A desktop browser, whose window a desktop can follow.
const ABLE: Abilities = {
  appleMedia: true,
  rdpGraphics: true,
  follows: "window",
};
const UNABLE: Abilities = {
  appleMedia: false,
  rdpGraphics: false,
  follows: "window",
};
const TABLET: Abilities = { ...ABLE, follows: "screen" };
const PHONE: Abilities = { ...ABLE, follows: null };
const DEFAULT_SIZE = { w: 1440, h: 900 };

function target(offers: Partial<TargetInfo>): TargetInfo {
  return {
    name: "t",
    protocol: "vnc",
    subtype: null,
    host: "192.0.2.1",
    port: 5900,
    resize: false,
    size: null,
    defaultSize: DEFAULT_SIZE,
    audio: false,
    passthrough: null,
    passthroughOnly: false,
    placement: false,
    ...offers,
  };
}

const RDP = target({
  protocol: "rdp",
  resize: true,
  audio: true,
  passthrough: "rdp-graphics",
});
const HIGH_PERFORMANCE = target({
  subtype: "ard-high-performance",
  resize: true,
  passthrough: "apple-media",
});

const SIZED = { w: 1920, h: 1080 };

function keys(rows: { key: string }[]): string[] {
  return rows.map((row) => row.key);
}

/** The formats a target's sound can be sent as, in the order shown. */
function formats(options: {
  soundRow: { formats: { value: string }[] } | null;
}): string[] | null {
  return options.soundRow?.formats.map((format) => format.value) ?? null;
}

/** The sizes a target offers a client, as `[value, label]`. */
function sizes(offered: TargetInfo, abilities: Abilities): string[][] {
  return targetOptions(offered, undefined, abilities).sizes.map((size) => [
    size.value,
    size.label,
  ]);
}

test("a target the window cannot drive keeps one size, on every client", () => {
  // A plain VNC server, and an RDP host on bitmap updates.
  for (const abilities of [ABLE, TABLET, PHONE]) {
    assert.deepEqual(sizes(target({}), abilities), [["target", "1440×900"]]);
    assert.deepEqual(sizes(target({ size: SIZED }), abilities), [
      ["target", "1920×1080"],
    ]);
  }
  // A plain server's answer to a size is not known before it is dialled.
  const plain = targetOptions(target({}), undefined, ABLE);
  assert.match(plain.sizes[0].note, /takes no size keeps its own/);
  assert.equal(plain.choices.size, "target");
});

test("a desktop browser and a tablet follow, beside a configured size", () => {
  assert.deepEqual(sizes(RDP, ABLE), [["window", "This window's size"]]);
  assert.deepEqual(sizes(RDP, TABLET), [["window", "This screen's size"]]);
  // The configured size comes first, so it is the size until somebody chooses.
  const sized = { ...RDP, size: SIZED };
  assert.deepEqual(sizes(sized, ABLE), [
    ["target", "1920×1080"],
    ["window", "This window's size"],
  ]);
  assert.deepEqual(sizes(sized, TABLET), [
    ["target", "1920×1080"],
    ["window", "This screen's size"],
  ]);
  assert.equal(targetOptions(sized, undefined, ABLE).choices.size, "target");
  assert.equal(targetOptions(RDP, undefined, ABLE).choices.size, "window");
});

test("a phone is offered sizes the desktop keeps, never its window", () => {
  assert.deepEqual(sizes(RDP, PHONE), [["target", "1440×900"]]);
  assert.deepEqual(sizes({ ...RDP, size: SIZED }, PHONE), [
    ["target", "1920×1080"],
    ["default", "1440×900"],
  ]);
});

test("a Mac sharing its physical displays is shown at their size", () => {
  const standard = targetOptions(
    target({ subtype: "ard", defaultSize: null }),
    undefined,
    PHONE,
  );
  assert.deepEqual(
    standard.sizes.map((size) => size.value),
    ["target"],
  );
  assert.match(standard.sizes[0].label, /own size/);
});

test("only what the target's type offers has a row", () => {
  const rdp = targetOptions(RDP, undefined, ABLE);
  assert.deepEqual(keys(rdp.rows), ["passthrough"]);
  assert.deepEqual(formats(rdp), ["opus", "flac"]);
  // A plain VNC server has nothing to choose.
  const plain = targetOptions(target({}), undefined, ABLE);
  assert.deepEqual(plain.rows, []);
  assert.equal(plain.soundRow, null);
  // wlshare's VP9 is the subtype's picture, not a choice. Its sound is.
  const wlshare = targetOptions(
    target({ subtype: "wlshare", resize: true, audio: true }),
    undefined,
    ABLE,
  );
  assert.deepEqual(wlshare.rows, []);
  assert.deepEqual(formats(wlshare), ["opus", "flac"]);
  // Standard Screen Sharing on the Mac's physical displays offers nothing.
  const standard = targetOptions(target({ subtype: "ard" }), undefined, ABLE);
  assert.deepEqual(standard.rows, []);
  assert.equal(standard.soundRow, null);
  assert.deepEqual(standard.choices, {
    size: "target",
    audio: "off",
    passthrough: false,
    placement: "right",
  });
  assert.equal(standard.blocked, null);
});

test("nothing is ticked until somebody ticks it", () => {
  const options = targetOptions(RDP, undefined, ABLE);
  assert.deepEqual(options.choices, {
    size: "window",
    audio: "off",
    passthrough: false,
    placement: "right",
  });
  assert.equal(options.sound, false);
  assert.ok(options.rows.every((row) => !row.checked && !row.disabled));
  assert.equal(options.soundRow?.checked, false);
});

test("what was chosen last time is what Start sends", () => {
  const sized = { ...RDP, size: SIZED };
  const options = targetOptions(sized, { size: "window", audio: "opus" }, ABLE);
  assert.deepEqual(options.choices, {
    size: "window",
    audio: "opus",
    passthrough: false,
    placement: "right",
  });
  assert.equal(options.sound, true);
  assert.equal(options.soundRow?.checked, true);
  // The format is part of the choice.
  const lossless = targetOptions(sized, { audio: "flac" }, ABLE);
  assert.equal(lossless.choices.audio, "flac");
  assert.equal(lossless.soundRow?.checked, true);
  assert.equal(lossless.sound, true);
  // A choice remembered for something the target does not offer here is not
  // sent: a phone has no window, and a plain server neither that nor sound.
  assert.equal(
    targetOptions(sized, { size: "window" }, PHONE).choices.size,
    "target",
  );
  const plain = targetOptions(
    target({}),
    { size: "window", audio: "opus", passthrough: true },
    ABLE,
  );
  assert.deepEqual(plain.choices, {
    size: "target",
    audio: "off",
    passthrough: false,
    placement: "right",
  });
  // Nor is something this page never wrote there.
  const stale = targetOptions(RDP, { audio: true as never }, ABLE);
  assert.equal(stale.choices.audio, "off");
});

test("High Performance's sound has no row and is always carried", () => {
  const options = targetOptions(HIGH_PERFORMANCE, undefined, ABLE);
  assert.deepEqual(keys(options.rows), ["passthrough"]);
  assert.equal(options.soundRow, null);
  assert.equal(options.choices.audio, "off");
  assert.equal(options.sound, true);
});

test("a passthrough this browser cannot take is greyed, with the reason", () => {
  for (const [offered, reason] of [
    [RDP, /WebGL 2/],
    [HIGH_PERFORMANCE, /does not decode/],
  ] as const) {
    // Remembered from a browser that could: still not sent from one that cannot.
    const options = targetOptions(offered, { passthrough: true }, UNABLE);
    const row = options.rows.find((r) => r.key === "passthrough");
    assert.ok(row);
    assert.equal(row.disabled, true);
    assert.equal(row.checked, false);
    assert.match(row.note, reason);
    assert.equal(options.choices.passthrough, false);
    assert.equal(options.blocked, null, "the target still starts, encoded");
  }
});

test("a gateway that cannot decode the Mac's picture can only pass it", () => {
  const only = { ...HIGH_PERFORMANCE, passthroughOnly: true };
  const able = targetOptions(only, { passthrough: false }, ABLE);
  const row = able.rows.find((r) => r.key === "passthrough");
  assert.ok(row);
  assert.equal(row.checked, true, "shown as the choice already made");
  assert.equal(row.disabled, true);
  assert.equal(able.choices.passthrough, true);
  assert.equal(able.blocked, null);

  // And where the browser cannot take it either, the target cannot start: Start
  // says so before the Mac is dialled.
  const unable = targetOptions(only, undefined, UNABLE);
  assert.match(unable.blocked ?? "", /cannot decode the Mac's picture/);
});

test("an RDP host's pipeline is never the only way, whatever the entry says", () => {
  // Every gateway composes the pipeline itself, so the flag means nothing here
  // and none of the Mac's wording reaches an RDP target.
  const flagged = { ...RDP, passthroughOnly: true };
  const able = targetOptions(flagged, { passthrough: false }, ABLE);
  const row = able.rows.find((r) => r.key === "passthrough");
  assert.ok(row);
  assert.equal(row.checked, false);
  assert.equal(row.disabled, false, "still a choice");
  assert.equal(able.blocked, null);

  const unable = targetOptions(flagged, undefined, UNABLE);
  assert.equal(unable.blocked, null, "the target still starts, encoded");
  assert.match(
    unable.rows.find((r) => r.key === "passthrough")?.note ?? "",
    /cannot compose/,
  );
});

const storage = new Map<string, string>();
beforeEach(() => {
  storage.clear();
  (globalThis as unknown as { localStorage: unknown }).localStorage = {
    getItem: (key: string) => storage.get(key) ?? null,
    setItem: (key: string, value: string) => storage.set(key, value),
  };
});

test("choices are remembered per target", () => {
  assert.deepEqual(readRememberedChoices(), {});
  let remembered = rememberChoice({}, "win", "audio", "flac");
  remembered = rememberChoice(remembered, "win", "size", "window");
  remembered = rememberChoice(remembered, "mac", "passthrough", true);
  remembered = rememberChoice(remembered, "win", "audio", "off");
  const expected = {
    win: { audio: "off", size: "window" },
    mac: { passthrough: true },
  };
  assert.deepEqual(remembered, expected);
  assert.deepEqual(readRememberedChoices(), expected, "and written through");
});

test("unreadable storage remembers nothing and still returns the choice", () => {
  storage.set("remotex.targetChoices", "not json");
  assert.deepEqual(readRememberedChoices(), {});
  (globalThis as unknown as { localStorage: unknown }).localStorage = {
    getItem: () => {
      throw new Error("blocked");
    },
    setItem: () => {
      throw new Error("blocked");
    },
  };
  assert.deepEqual(readRememberedChoices(), {});
  assert.deepEqual(rememberChoice({}, "win", "size", "window"), {
    win: { size: "window" },
  });
});

test("the second display's place is a choice where the target offers it", () => {
  // Not offered, there is no row and it is where it has always been, whatever
  // was remembered.
  const one = targetOptions(RDP, { placement: "left" }, ABLE);
  assert.equal(one.placements, null);
  assert.equal(one.choices.placement, "right");
  const two = { ...RDP, placement: true };
  const fresh = targetOptions(two, undefined, ABLE);
  assert.deepEqual(
    fresh.placements?.map((option) => option.value),
    ["right", "left", "top", "bottom"],
  );
  assert.equal(fresh.choices.placement, "right");
  assert.equal(
    targetOptions(two, { placement: "bottom" }, ABLE).choices.placement,
    "bottom",
  );
  // Nor is something this page never wrote there.
  const stale = targetOptions(two, { placement: "above" as never }, ABLE);
  assert.equal(stale.choices.placement, "right");
});
