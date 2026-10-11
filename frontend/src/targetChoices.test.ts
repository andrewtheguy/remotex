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
  audio: true,
  runsHevc: true,
  follows: "window",
};
const UNABLE: Abilities = {
  appleMedia: false,
  rdpGraphics: false,
  audio: false,
  runsHevc: false,
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
    software: false,
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
    software: false,
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
    software: false,
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
    software: false,
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
    software: false,
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

test("a browser with no audio decoder greys the sound and sends it off", () => {
  const MUTE: Abilities = { ...ABLE, audio: false };
  // Remembered as ticked, from a browser that could: kept, and not sent.
  for (const remembered of ["opus", "flac"] as const) {
    const options = targetOptions(RDP, { audio: remembered }, MUTE);
    assert.equal(options.soundRow?.disabled, true);
    assert.equal(options.soundRow?.checked, false);
    assert.match(options.soundRow?.note ?? "", /AudioDecoder/);
    assert.equal(options.choices.audio, "off");
    assert.equal(options.sound, false);
  }
  // The same target in a browser that can is as it was left.
  const able = targetOptions(RDP, { audio: "flac" }, ABLE);
  assert.equal(able.soundRow?.disabled, false);
  assert.equal(able.choices.audio, "flac");
  // High Performance's sound comes whatever was chosen, and nothing here can
  // play it, so Start spends no click on an audio context.
  assert.equal(targetOptions(HIGH_PERFORMANCE, undefined, MUTE).sound, false);
  // The desktop itself is untouched: a passthrough this browser takes is offered.
  assert.equal(
    targetOptions(HIGH_PERFORMANCE, undefined, MUTE).rows[0]?.disabled,
    false,
  );
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

// BETA: decoding a Mac's passed HEVC in this page.
const MAC_PAGE = { ...HIGH_PERFORMANCE, software: true };
/** A browser whose own decoder does not take the Mac's HEVC, and whose page does. */
const NEEDS_PAGE: Abilities = { ...ABLE, appleMedia: false };

function softwareRow(options: {
  rows: {
    key: string;
    label: string;
    checked: boolean;
    disabled: boolean;
    note: string;
  }[];
}) {
  return options.rows.find((row) => row.key === "software") ?? null;
}

test("decoding in this page has a row only while a Mac's picture the gateway decodes for it is passed", () => {
  // No target but a Mac has HEVC to pass, and the VP9 is no choice.
  assert.equal(softwareRow(targetOptions(RDP, undefined, NEEDS_PAGE)), null);
  assert.equal(
    targetOptions(RDP, { software: true }, NEEDS_PAGE).choices.software,
    false,
    "remembered from a Mac: not sent for another target",
  );
  // A Mac's picture encoded here is VP9, which the page decodes however it can.
  const encoded = targetOptions(MAC_PAGE, { passthrough: false }, NEEDS_PAGE);
  assert.equal(softwareRow(encoded), null);
  assert.equal(encoded.choices.software, false);
  // A gateway without the page's decoder offers none.
  const without = targetOptions(HIGH_PERFORMANCE, { passthrough: true }, ABLE);
  assert.equal(softwareRow(without), null);
  const passed = targetOptions(MAC_PAGE, { passthrough: true }, ABLE);
  assert.match(softwareRow(passed)?.label ?? "", /^Decode HEVC in this page/);
});

test("decoding in this page is a choice where the browser's own decoder takes the Mac's HEVC", () => {
  for (const [remembered, checked] of [
    [undefined, false],
    [true, true],
    [false, false],
  ] as const) {
    const options = targetOptions(
      MAC_PAGE,
      { passthrough: true, software: remembered },
      ABLE,
    );
    const row = softwareRow(options);
    assert.ok(row);
    assert.equal(row.disabled, false);
    assert.equal(row.checked, checked);
    assert.equal(options.choices.software, checked);
  }
});

test("decoding in this page is greyed where this page cannot run the decoder", () => {
  const cannot = { ...ABLE, runsHevc: false };
  const options = targetOptions(
    MAC_PAGE,
    { passthrough: true, software: true },
    cannot,
  );
  const row = softwareRow(options);
  assert.ok(row);
  assert.equal(row.disabled, true);
  assert.equal(row.checked, false);
  assert.match(row.note, /cannot run/);
  assert.equal(options.choices.software, false);
});

test("a Mac's picture only this page decodes can be passed, and is then decoded in this page", () => {
  // The browser's own decoder refuses the HEVC; the gateway serves the page's.
  const options = targetOptions(MAC_PAGE, { passthrough: true }, NEEDS_PAGE);
  const passed = options.rows.find((row) => row.key === "passthrough");
  assert.equal(passed?.disabled, false);
  assert.equal(passed?.checked, true);
  const row = softwareRow(options);
  assert.ok(row);
  assert.equal(row.checked, true);
  assert.equal(row.disabled, true, "not a choice: nothing else decodes it");
  assert.equal(options.choices.software, true);
  // Unticking it is remembered and not sent: the passthrough depends on it.
  const forced = targetOptions(
    MAC_PAGE,
    { passthrough: true, software: false },
    NEEDS_PAGE,
  );
  assert.equal(forced.choices.software, true);

  // Without the page's HEVC decoder, served or runnable, the passthrough is
  // greyed as for any browser that cannot take it.
  for (const [offered, abilities] of [
    [HIGH_PERFORMANCE, NEEDS_PAGE],
    [MAC_PAGE, { ...NEEDS_PAGE, runsHevc: false }],
  ] as const) {
    const greyed = targetOptions(offered, { passthrough: true }, abilities);
    const passthrough = greyed.rows.find((r) => r.key === "passthrough");
    assert.equal(passthrough?.disabled, true);
    assert.equal(greyed.choices.passthrough, false);
  }

  // A gateway that can only pass the picture starts for such a browser too.
  const only = { ...MAC_PAGE, passthroughOnly: true };
  const started = targetOptions(only, undefined, NEEDS_PAGE);
  assert.equal(started.blocked, null);
  assert.deepEqual(
    [started.choices.passthrough, started.choices.software],
    [true, true],
  );
});
