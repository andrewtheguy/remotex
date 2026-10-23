// Which options a target shows at the picker, which are greyed, and what Start
// then sends. The properties under test are the picker's rules: the size a session
// will have is always shown and is a choice only where there are two, not offered
// is not shown, offered but unavailable is greyed with the reason, and a target
// whose stream is passed in every session cannot start in a browser that cannot
// take it.
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
  rdpH264: true,
  appleH264: true,
  vp9: true,
  audio: true,
  lossless: true,
  runsHevc: true,
  follows: "window",
};
const UNABLE: Abilities = {
  appleMedia: false,
  rdpGraphics: false,
  rdpH264: false,
  appleH264: false,
  vp9: false,
  audio: false,
  lossless: false,
  runsHevc: false,
  follows: "window",
};
// An iPad's and an iPhone's browser, whose desktop follows the screen or nothing.
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
    passthroughs: [],
    placement: false,
    pipeline: false,
    standardView: false,
    camera: false,
    microphone: false,
    ...offers,
  };
}

// As the gateway lists an RDP target with its pipeline on: passed in every session.
const RDP = target({
  protocol: "rdp",
  resize: true,
  audio: true,
  passthroughs: ["rdp-graphics"],
  pipeline: true,
  camera: true,
  microphone: true,
});
const HIGH_PERFORMANCE = target({
  subtype: "ard-high-performance",
  resize: true,
  passthroughs: ["apple-media", "apple-h264"],
});

const STANDARD = target({
  subtype: "ard",
  passthroughs: ["apple-adaptive"],
  standardView: true,
});

// As the gateway lists a `wlshare` target: its VP9 and its tiles with motion,
// both wlshare's own, passed.
const WLSHARE = target({
  subtype: "wlshare",
  resize: true,
  audio: true,
  passthroughs: ["wlshare-vp9", "wlshare-tiles-motion"],
  camera: true,
  microphone: true,
});

const SIZED = { w: 1920, h: 1080 };

/** A target's picture options as value and whether it is greyed, in the order shown. */
function pictures(options: {
  pictures: { value: string | null; disabled: boolean }[] | null;
}): [string | null, boolean][] | null {
  return (
    options.pictures?.map((picture) => [picture.value, picture.disabled]) ??
    null
  );
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
  // An RDP host on bitmap updates.
  const bitmap = { protocol: "rdp" as const, port: 3389 };
  for (const abilities of [ABLE, TABLET, PHONE]) {
    assert.deepEqual(sizes(target(bitmap), abilities), [
      ["target", "1440×900 (default)"],
    ]);
    assert.deepEqual(sizes(target({ ...bitmap, size: SIZED }), abilities), [
      ["target", "1920×1080"],
    ]);
  }
});

test("a target that states no size is shown at its server's own", () => {
  // A plain VNC server, and a Mac sharing its physical displays.
  for (const abilities of [ABLE, TABLET, PHONE]) {
    const plain = targetOptions(
      target({ defaultSize: null }),
      undefined,
      abilities,
    );
    assert.deepEqual(
      plain.sizes.map((s) => [s.value, s.label]),
      [["target", "The remote's own size"]],
    );
    assert.equal(plain.choices.size, "target");
  }
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
  assert.deepEqual(sizes(RDP, PHONE), [["target", "1440×900 (default)"]]);
  assert.deepEqual(sizes({ ...RDP, size: SIZED }, PHONE), [
    ["target", "1920×1080"],
    ["default", "1440×900 (default)"],
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
  // Its pipeline is passed in every session, which is said rather than offered.
  assert.equal(rdp.pictures, null);
  assert.ok(rdp.passed);
  assert.deepEqual(formats(rdp), ["opus", "flac"]);
  // A plain VNC server has nothing to choose.
  const plain = targetOptions(target({}), undefined, ABLE);
  assert.equal(plain.pictures, null);
  assert.equal(plain.soundRow, null);
  // wlshare's picture is one of its two, its VP9 first, and its sound a choice.
  const wlshare = targetOptions(WLSHARE, undefined, ABLE);
  assert.deepEqual(pictures(wlshare), [
    ["wlshare-vp9", false],
    ["wlshare-tiles-motion", false],
  ]);
  assert.equal(wlshare.choices.passthrough, "wlshare-vp9");
  assert.deepEqual(formats(wlshare), ["opus", "flac"]);
  // Standard Screen Sharing on the Mac's physical displays offers its picture
  // alone: the Mac's Adaptive one passed, until the gateway's tiles are chosen.
  const standard = targetOptions(STANDARD, undefined, ABLE);
  assert.deepEqual(pictures(standard), [
    ["apple-adaptive", false],
    [null, false],
  ]);
  assert.equal(standard.soundRow, null);
  assert.deepEqual(standard.choices, {
    size: "target",
    audio: "off",
    passthrough: "apple-adaptive",
    placement: "right",
    pipeline: "high-quality",
    standardView: "one",
    camera: false,
    microphone: false,
  });
  assert.equal(standard.blocked, null);
});

test("a Standard Mac's picture is its Adaptive picture passed, or the gateway's tiles", () => {
  const first = targetOptions(STANDARD, undefined, UNABLE);
  assert.deepEqual(
    pictures(first),
    [
      ["apple-adaptive", false],
      [null, false],
    ],
    "a browser that takes no other passthrough takes these",
  );
  assert.equal(first.choices.passthrough, "apple-adaptive", "the first");
  assert.match(first.pictures?.[0].label ?? "", /^Apple's Adaptive \(beta\)/);
  assert.match(first.pictures?.[1].label ?? "", /^Gateway adaptive/);
  assert.match(first.pictures?.[1].note ?? "", /exact once it has settled/);
  // Nothing remembered is the first; a remembered null is the gateway's tiles.
  assert.equal(
    targetOptions(STANDARD, { standardView: "one" }, ABLE).choices.passthrough,
    "apple-adaptive",
  );
  const tiles = targetOptions(STANDARD, { passthrough: null }, UNABLE);
  assert.equal(tiles.choices.passthrough, null);
  assert.equal(tiles.blocked, null);

  const adaptive = targetOptions(
    STANDARD,
    { passthrough: "apple-adaptive" },
    ABLE,
  );
  assert.equal(adaptive.choices.passthrough, "apple-adaptive");
  assert.equal(adaptive.blocked, null);
  assert.equal(adaptive.sound, false);

  // On every other target a remembered null is nothing chosen.
  assert.equal(
    targetOptions(HIGH_PERFORMANCE, { passthrough: null }, ABLE).choices
      .passthrough,
    "apple-media",
  );
});

test("a Standard Mac's screens are one at a time or Combined Display", () => {
  const one = targetOptions(STANDARD, undefined, ABLE);
  assert.deepEqual(
    one.views?.map((view) => view.value),
    ["one", "combined"],
  );
  assert.match(one.views?.[0].label ?? "", /^One at a time \(beta\)/);
  assert.match(one.views?.[1].label ?? "", /^Combined \(alpha\)/);
  assert.equal(one.choices.standardView, "one", "until Combined is chosen");
  // Combined Display is the gateway's tiles: the Mac's Adaptive picture is
  // greyed under it, and comes back when one screen at a time is chosen again.
  const combined = targetOptions(
    STANDARD,
    { standardView: "combined", passthrough: "apple-adaptive" },
    ABLE,
  );
  assert.equal(combined.choices.standardView, "combined");
  assert.equal(combined.choices.passthrough, null);
  assert.deepEqual(pictures(combined), [
    ["apple-adaptive", true],
    [null, false],
  ]);
  assert.equal(combined.blocked, null);
  assert.equal(
    targetOptions(STANDARD, { passthrough: "apple-adaptive" }, ABLE).choices
      .passthrough,
    "apple-adaptive",
  );
  // Screen Sharing Standard's alone: no other target has the row, and a
  // Combined remembered for it is not sent.
  for (const other of [RDP, HIGH_PERFORMANCE, target({})]) {
    const options = targetOptions(other, { standardView: "combined" }, ABLE);
    assert.equal(options.views, null);
    assert.equal(options.choices.standardView, "one");
  }
});

test("a Mac's picture is its HEVC passed or its H.264 passed", () => {
  const options = targetOptions(HIGH_PERFORMANCE, undefined, ABLE);
  assert.deepEqual(pictures(options), [
    ["apple-media", false],
    ["apple-h264", false],
  ]);
  assert.equal(options.choices.passthrough, "apple-media", "the first");
  assert.equal(options.passed, null);
  for (const chosen of ["apple-media", "apple-h264"] as const) {
    const again = targetOptions(
      HIGH_PERFORMANCE,
      { passthrough: chosen },
      ABLE,
    );
    assert.equal(again.choices.passthrough, chosen);
  }
  // Something this page never wrote there is not a choice, and nor is the
  // picture encoded here a Mac's once had.
  for (const stale of [true as never, null]) {
    const again = targetOptions(HIGH_PERFORMANCE, { passthrough: stale }, ABLE);
    assert.equal(again.choices.passthrough, "apple-media");
  }
});

test("no sound is taken until somebody ticks it", () => {
  const rdp = targetOptions(RDP, undefined, ABLE);
  assert.deepEqual(rdp.choices, {
    size: "window",
    audio: "off",
    passthrough: null,
    placement: "right",
    pipeline: "high-quality",
    standardView: "one",
    camera: false,
    microphone: false,
  });
  assert.equal(rdp.sound, false);
  assert.equal(rdp.soundRow?.checked, false);
});

test("what was chosen last time is what Start sends", () => {
  const sized = { ...RDP, size: SIZED };
  const options = targetOptions(sized, { size: "window", audio: "opus" }, ABLE);
  assert.deepEqual(options.choices, {
    size: "window",
    audio: "opus",
    passthrough: null,
    placement: "right",
    pipeline: "high-quality",
    standardView: "one",
    camera: false,
    microphone: false,
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
    { size: "window", audio: "opus", passthrough: "apple-h264" },
    ABLE,
  );
  assert.deepEqual(plain.choices, {
    size: "target",
    audio: "off",
    passthrough: null,
    placement: "right",
    pipeline: "high-quality",
    standardView: "one",
    camera: false,
    microphone: false,
  });
  // Nor is something this page never wrote there.
  const stale = targetOptions(RDP, { audio: true as never }, ABLE);
  assert.equal(stale.choices.audio, "off");
});

test("High Performance's sound has no row and is always carried", () => {
  const options = targetOptions(HIGH_PERFORMANCE, undefined, ABLE);
  assert.equal(options.pictures?.length, 2);
  assert.equal(options.soundRow, null);
  assert.equal(options.choices.audio, "off");
  assert.equal(options.sound, true);
});

test("Opus says its rate follows the link", () => {
  const note = targetOptions(RDP, undefined, ABLE).soundRow?.formats[0].note;
  assert.match(note ?? "", /up to 320 kbit\/s, lower while the link is behind/);
});

test("a browser with no audio decoder is offered no sound", () => {
  // Safari 16.4 to 18 as the page sees it: a video decoder and no audio one.
  const MUTE: Abilities = { ...ABLE, audio: false, lossless: false };
  const formats = (options: ReturnType<typeof targetOptions>) =>
    options.soundRow?.formats.map((format) => [format.value, format.disabled]);

  // Nothing plays: the sound cannot be ticked, and says why.
  for (const remembered of [undefined, "opus", "flac"] as const) {
    const options = targetOptions(RDP, { audio: remembered }, MUTE);
    assert.equal(options.soundRow?.disabled, true);
    assert.equal(options.soundRow?.checked, false);
    assert.equal(options.choices.audio, "off");
    assert.equal(options.sound, false);
    assert.deepEqual(formats(options), [
      ["opus", true],
      ["flac", true],
    ]);
    assert.match(options.soundRow?.note ?? "", /AudioDecoder/);
  }

  // High Performance's sound is its AAC-ELD, which nothing here can play, so
  // Start spends no click on an audio context. Its picture is untouched.
  const mac = targetOptions(HIGH_PERFORMANCE, undefined, MUTE);
  assert.equal(mac.sound, false);
  assert.equal(mac.pictures?.[0].disabled, false);
});

test("a browser whose decoder does not play FLAC is offered Opus alone", () => {
  // Safari 27, whose AudioDecoder takes Opus and fails a FLAC frame.
  const SAFARI: Abilities = { ...ABLE, lossless: false };
  const formats = (options: ReturnType<typeof targetOptions>) =>
    options.soundRow?.formats.map((format) => [format.value, format.disabled]);

  // FLAC is greyed, with the reason.
  const options = targetOptions(RDP, undefined, SAFARI);
  assert.equal(options.soundRow?.disabled, false);
  assert.deepEqual(formats(options), [
    ["opus", false],
    ["flac", true],
  ]);
  assert.match(options.soundRow?.formats[1].note ?? "", /does not decode FLAC/);

  // A FLAC remembered from before is not sent, and is kept for a browser that
  // plays it; an Opus one is.
  const lossless = targetOptions(RDP, { audio: "flac" }, SAFARI);
  assert.equal(lossless.choices.audio, "off");
  assert.equal(lossless.soundRow?.checked, false);
  assert.equal(lossless.sound, false);
  assert.equal(
    targetOptions(RDP, { audio: "flac" }, ABLE).choices.audio,
    "flac",
  );
  assert.equal(
    targetOptions(RDP, { audio: "opus" }, SAFARI).choices.audio,
    "opus",
  );
  assert.deepEqual(formats(targetOptions(RDP, undefined, ABLE)), [
    ["opus", false],
    ["flac", false],
  ]);
});

test("a Mac's HEVC this browser cannot take is greyed, with the reason", () => {
  // Remembered from a browser that could: still not sent from one that cannot.
  // The H.264 is what is left, and the target starts with it.
  const options = targetOptions(
    HIGH_PERFORMANCE,
    { passthrough: "apple-media" },
    { ...UNABLE, appleH264: true },
  );
  assert.deepEqual(pictures(options), [
    ["apple-media", true],
    ["apple-h264", false],
  ]);
  assert.match(options.pictures?.[0].note ?? "", /does not decode/);
  assert.equal(
    options.choices.passthrough,
    "apple-h264",
    "the first it can have",
  );
  assert.equal(options.blocked, null);
});

test("a Mac's H.264 this browser cannot take is greyed, with the reason", () => {
  const options = targetOptions(
    HIGH_PERFORMANCE,
    { passthrough: "apple-h264" },
    { ...ABLE, appleH264: false },
  );
  assert.deepEqual(pictures(options), [
    ["apple-media", false],
    ["apple-h264", true],
  ]);
  assert.match(
    options.pictures?.[1].note ?? "",
    /no video decoder for the Mac's H.264/,
  );
  assert.equal(
    options.choices.passthrough,
    "apple-media",
    "the one it can have",
  );
  assert.equal(options.blocked, null);
});

test("a Mac neither of whose pictures this browser takes cannot start", () => {
  // A browser with no VideoDecoder, on a gateway whose page cannot decode the
  // HEVC either: the Mac sends nothing else.
  const options = targetOptions(HIGH_PERFORMANCE, undefined, UNABLE);
  assert.deepEqual(pictures(options), [
    ["apple-media", true],
    ["apple-h264", true],
  ]);
  assert.equal(options.choices.passthrough, null);
  assert.match(options.blocked ?? "", /neither of the Mac's pictures/);
});

test("a wlshare target's picture is its VP9 passed or its tiles with motion passed", () => {
  // The VP9 until the tiles are chosen, and the choice is remembered; neither is
  // greyed, since both are VP9 and the target cannot start without it.
  const options = targetOptions(WLSHARE, undefined, ABLE);
  assert.deepEqual(pictures(options), [
    ["wlshare-vp9", false],
    ["wlshare-tiles-motion", false],
  ]);
  assert.equal(options.choices.passthrough, "wlshare-vp9", "the first");
  const tiles = targetOptions(
    WLSHARE,
    { passthrough: "wlshare-tiles-motion" },
    ABLE,
  );
  assert.equal(tiles.choices.passthrough, "wlshare-tiles-motion");
  assert.equal(tiles.blocked, null);
  // A picture remembered from another target's kind is not this one's.
  assert.equal(
    targetOptions(WLSHARE, { passthrough: "apple-media" }, ABLE).choices
      .passthrough,
    "wlshare-vp9",
  );
});

test("the camera and the microphone are offered where the remote can take them, off until ticked", () => {
  // An RDP host and wlshare have somewhere to put them; a plain server and a
  // Mac do not, and have no row.
  for (const target of [RDP, WLSHARE]) {
    const options = targetOptions(target, undefined, ABLE);
    assert.equal(options.cameraRow?.checked, false);
    assert.equal(options.microphoneRow?.checked, false);
    assert.match(options.cameraRow?.label ?? "", /experimental/);
    assert.equal(options.choices.camera, false);
    assert.equal(options.choices.microphone, false);
    const ticked = targetOptions(
      target,
      { camera: true, microphone: true },
      ABLE,
    );
    assert.equal(ticked.cameraRow?.checked, true);
    assert.equal(ticked.microphoneRow?.checked, true);
    assert.equal(ticked.choices.camera, true);
    assert.equal(ticked.choices.microphone, true);
  }
  for (const other of [target({}), STANDARD, HIGH_PERFORMANCE]) {
    const options = targetOptions(
      other,
      { camera: true, microphone: true },
      ABLE,
    );
    assert.equal(options.cameraRow, null);
    assert.equal(options.microphoneRow, null);
    // Remembered from another kind of target, and not sent to one with no row.
    assert.equal(options.choices.camera, false);
    assert.equal(options.choices.microphone, false);
  }
});

test("a wlshare target needs a browser that decodes its VP9", () => {
  assert.equal(targetOptions(WLSHARE, undefined, ABLE).blocked, null);
  const unable = targetOptions(WLSHARE, undefined, { ...ABLE, vp9: false });
  assert.match(unable.blocked ?? "", /does not decode VP9/);
  // A plain target, read through the RFB baseline, is sent the tiles.
  assert.equal(targetOptions(target({}), undefined, UNABLE).blocked, null);
});

test("a Standard Mac starts in a browser with no video decoder", () => {
  // Its Adaptive picture is decoded by the page, and its Full quality is the tiles.
  const options = targetOptions(STANDARD, undefined, UNABLE);
  assert.deepEqual(pictures(options), [
    ["apple-adaptive", false],
    [null, false],
  ]);
  assert.equal(options.blocked, null);
});

test("an RDP host's pipeline is always passed, and needs a page that composes it", () => {
  // No gateway composes the pipeline, so it is shown as passed, whatever was
  // remembered.
  const able = targetOptions(RDP, { passthrough: "apple-media" }, ABLE);
  assert.equal(able.pictures, null, "no choice");
  assert.equal(able.passed?.label, "Always passed through");
  assert.match(able.passed?.note ?? "", /egfx is on/);
  assert.equal(able.choices.passthrough, null, "passed without being named");
  assert.equal(able.blocked, null);

  // A browser that cannot compose it cannot start the target: no VP9 stands in.
  const unable = targetOptions(RDP, undefined, UNABLE);
  assert.match(unable.blocked ?? "", /WebGL 2/);
  assert.ok(unable.passed);
});

test("how the host draws the pipeline is high quality until somebody chooses adaptive", () => {
  const options = targetOptions(RDP, undefined, ABLE);
  assert.equal(options.choices.pipeline, "high-quality");
  assert.deepEqual(
    options.pipelineRow?.modes.map((mode) => [mode.value, mode.disabled]),
    [
      ["high-quality", false],
      ["adaptive", false],
    ],
  );
  assert.match(options.pipelineRow?.modes[1].label ?? "", /alpha/);
  assert.deepEqual(
    options.pipelineRow?.h264.map((option) => option.value),
    ["adaptive-desktop", "adaptive-region"],
  );
  for (const pipeline of ["adaptive-desktop", "adaptive-region"] as const) {
    assert.equal(
      targetOptions(RDP, { pipeline }, ABLE).choices.pipeline,
      pipeline,
    );
  }
  // No other target has a pipeline to draw.
  const mac = targetOptions(
    HIGH_PERFORMANCE,
    { pipeline: "adaptive-desktop" },
    ABLE,
  );
  assert.equal(mac.pipelineRow, null);
  assert.equal(mac.choices.pipeline, "high-quality");
});

test("a browser that cannot decode the H.264 is offered high quality alone", () => {
  const NO_H264: Abilities = { ...ABLE, rdpH264: false };
  const options = targetOptions(RDP, { pipeline: "adaptive-region" }, NO_H264);
  const adaptive = options.pipelineRow?.modes[1];
  assert.equal(adaptive?.disabled, true);
  assert.match(adaptive?.note ?? "", /VideoDecoder/);
  assert.equal(
    options.choices.pipeline,
    "high-quality",
    "remembered, not sent",
  );
  assert.equal(options.blocked, null, "the target still starts, high quality");
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
  remembered = rememberChoice(remembered, "mac", "passthrough", "apple-h264");
  remembered = rememberChoice(remembered, "win", "audio", "off");
  const expected = {
    win: { audio: "off", size: "window" },
    mac: { passthrough: "apple-h264" },
  };
  assert.deepEqual(remembered, expected);
  assert.deepEqual(readRememberedChoices(), expected, "and written through");
});

test("unreadable storage remembers nothing and still returns the choice", () => {
  storage.set("zdesk.targetChoices", "not json");
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

// A Mac's passed HEVC decoded in this page where the browser's own decoder
// does not take it.
const MAC_PAGE = { ...HIGH_PERFORMANCE, software: true };
/** A browser whose own decoder does not take the Mac's HEVC, and whose page does. */
const NEEDS_PAGE: Abilities = { ...ABLE, appleMedia: false };

test("decoding in this page is no option: the passthrough is all that is chosen", () => {
  for (const abilities of [ABLE, NEEDS_PAGE]) {
    const options = targetOptions(
      MAC_PAGE,
      { passthrough: "apple-media" },
      abilities,
    );
    assert.deepEqual(pictures(options), [
      ["apple-media", false],
      ["apple-h264", false],
    ]);
    assert.equal(options.choices.passthrough, "apple-media");
  }
});

test("a Mac's picture only this page decodes can be passed", () => {
  // The browser's own decoder refuses the HEVC; the gateway serves the page's.
  const options = targetOptions(MAC_PAGE, undefined, NEEDS_PAGE);
  assert.equal(options.pictures?.[0].disabled, false);
  assert.equal(options.choices.passthrough, "apple-media");

  // Without the page's HEVC decoder, served or runnable, the HEVC is greyed as
  // for any browser that cannot take it, with which is missing.
  for (const [offered, abilities, reason] of [
    [HIGH_PERFORMANCE, NEEDS_PAGE, /this gateway serves no decoder/],
    [MAC_PAGE, { ...NEEDS_PAGE, runsHevc: false }, /needs WebGL 2/],
  ] as const) {
    const greyed = targetOptions(
      offered,
      { passthrough: "apple-media" },
      abilities,
    );
    assert.equal(greyed.pictures?.[0].disabled, true);
    assert.match(greyed.pictures?.[0].note ?? "", reason);
    assert.equal(greyed.choices.passthrough, "apple-h264");
  }
});
