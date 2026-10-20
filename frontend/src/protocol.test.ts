// The click count the browser puts on the wire. Pure bounds logic, so it is
// checked here; what a browser actually reports in `MouseEvent.detail` is its
// own double-click policy and no unit test can stand in for it.
//
// It matters more than a bounds check looks: a value of zero is a click that
// counts as no click at all where a guest infers double-clicks from the count.
// Run with `bun test src/protocol.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  batchFrameSequence,
  clickCount,
  decodeAudioFrame,
  decodeBatchFrame,
  mouseButtonBit,
  mouseButtonFromEvent,
  wheelFromEvent,
  wheelUnitFromEvent,
} from "./protocol.ts";

test("a screen batch carries its attachment-local sequence", () => {
  const frame = new Uint8Array([0x02, 0, 0, 0, 0x78, 0x56, 0x34, 0x12]);
  assert.equal(batchFrameSequence(frame.buffer), 0x12345678);
  assert.equal(batchFrameSequence(frame.slice(0, 7).buffer), null);

  frame[0] = 0x03;
  assert.equal(batchFrameSequence(frame.buffer), null);
  frame[0] = 0x02;
  frame[1] = 1;
  assert.equal(batchFrameSequence(frame.buffer), null);
});

test("screen batch sequences start at one; zero is not an attachment sequence", () => {
  const frame = new Uint8Array([0x02, 0, 0, 0, 0, 0, 0, 0]);
  assert.equal(batchFrameSequence(frame.buffer), null);
  frame[4] = 1;
  assert.equal(batchFrameSequence(frame.buffer), 1);
});

test("a graphics record is its commands, whole, among the records around it", () => {
  // Transcribed from `batch` in src/protocol.rs: op 0x04, a u32 length, the commands.
  const frame = new Uint8Array([
    0x02, 0x00, 0x02, 0x00, 0x05, 0x00, 0x00, 0x00, 0x04, 0x03, 0x00, 0x00,
    0x00, 0xaa, 0xbb, 0xcc, 0x04, 0x01, 0x00, 0x00, 0x00, 0xdd,
  ]).buffer;
  const records = decodeBatchFrame(frame);
  assert.deepEqual(
    records?.map((record) => [record.kind, [...record.data]]),
    [
      ["graphics", [0xaa, 0xbb, 0xcc]],
      ["graphics", [0xdd]],
    ],
  );
  // Cut short, and with a length of nothing: neither is a smaller record.
  assert.equal(decodeBatchFrame(frame.slice(0, frame.byteLength - 1)), null);
  const empty = new Uint8Array([
    0x02, 0x00, 0x01, 0x00, 0x05, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
    0x00,
  ]).buffer;
  assert.equal(decodeBatchFrame(empty), null);
});

test("a standard record is where its rectangle is, then its body, whole", () => {
  // Transcribed from `StandardUnit::write_record` in src/protocol.rs: op 0x05, x,
  // y, w and h as u16, a u32 length, the body, little-endian.
  const frame = new Uint8Array([
    0x02, 0x00, 0x01, 0x00, 0x09, 0x00, 0x00, 0x00, 0x05, 0x08, 0x00, 0x10,
    0x00, 0x00, 0x01, 0x18, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x02,
  ]).buffer;
  const records = decodeBatchFrame(frame);
  assert.equal(records?.length, 1);
  const record = records?.[0];
  assert.equal(record?.kind, "standard");
  if (record?.kind === "standard") {
    assert.deepEqual(
      [record.x, record.y, record.w, record.h, [...record.data]],
      [8, 16, 256, 24, [1, 2]],
    );
  }
  assert.equal(decodeBatchFrame(frame.slice(0, frame.byteLength - 1)), null);
  const empty = new Uint8Array([
    0x02, 0x00, 0x01, 0x00, 0x09, 0x00, 0x00, 0x00, 0x05, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0,
  ]).buffer;
  assert.equal(decodeBatchFrame(empty), null, "a rectangle with no body");
});

test("a tile record is its format, where its rectangle is, then its image, whole", () => {
  // Transcribed from `TileUnit::write_record` in src/protocol.rs: op 0x06, the
  // format byte, x, y, w and h as u16, a u32 length, the image, little-endian.
  const frame = (format: number, w: number, image: number[]) =>
    new Uint8Array([
      0x02,
      0x00,
      0x01,
      0x00,
      0x09,
      0x00,
      0x00,
      0x00,
      0x06,
      format,
      0x40,
      0x00,
      0x80,
      0x00,
      w,
      0x00,
      0x24,
      0x00,
      image.length,
      0x00,
      0x00,
      0x00,
      ...image,
    ]).buffer;
  const records = decodeBatchFrame(frame(2, 64, [0x89, 0x50]));
  assert.equal(records?.length, 1);
  const record = records?.[0];
  assert.equal(record?.kind, "tile");
  if (record?.kind === "tile") {
    assert.deepEqual(
      [record.format, record.x, record.y, record.w, record.h, [...record.data]],
      ["png", 64, 128, 64, 36, [0x89, 0x50]],
    );
  }
  const jpeg = decodeBatchFrame(frame(1, 64, [0xff]))?.[0];
  assert.equal(jpeg?.kind === "tile" && jpeg.format, "jpeg");
  const whole = frame(2, 64, [0x89, 0x50]);
  assert.equal(decodeBatchFrame(whole.slice(0, whole.byteLength - 1)), null);
  assert.equal(
    decodeBatchFrame(frame(3, 64, [1])),
    null,
    "a format this client does not know",
  );
  assert.equal(decodeBatchFrame(frame(2, 0, [1])), null, "an empty rectangle");
  assert.equal(
    decodeBatchFrame(frame(2, 64, [])),
    null,
    "a rectangle with no image",
  );
});

test("a region record is its stream, its flags, where its rectangle is, then its unit, whole", () => {
  // Transcribed from `RegionUnit::write_record` in src/protocol.rs: op 0x07,
  // the stream, the flags (0x01 a keyframe), x, y, w and h as u16, a u32 length,
  // the unit, little-endian.
  const frame = (flags: number, w: number, unit: number[]) =>
    new Uint8Array([
      0x02,
      0x00,
      0x02,
      0x00,
      0x09,
      0x00,
      0x00,
      0x00,
      0x07,
      0x03,
      flags,
      0x00,
      0x01,
      0x2c,
      0x01,
      w & 0xff,
      w >> 8,
      0xc9,
      0x00,
      unit.length,
      0x00,
      0x00,
      0x00,
      ...unit,
      // A tile after it, which a region's length must not run into.
      0x06,
      0x02,
      0x00,
      0x00,
      0x00,
      0x00,
      0x08,
      0x00,
      0x08,
      0x00,
      0x01,
      0x00,
      0x00,
      0x00,
      0x89,
    ]).buffer;
  const records = decodeBatchFrame(frame(0x01, 0x0101, [0xaa, 0xbb]));
  assert.deepEqual(records?.[0], {
    kind: "region",
    stream: 3,
    x: 256,
    y: 300,
    w: 257,
    h: 201,
    keyframe: true,
    data: new Uint8Array([0xaa, 0xbb]),
  });
  assert.equal(records?.[1]?.kind, "tile");
  const delta = decodeBatchFrame(frame(0x00, 0x0101, [0xaa]))?.[0];
  assert.equal(delta?.kind === "region" && delta.keyframe, false);
  for (const flags of [0x02, 0x80, 0x81]) {
    assert.equal(
      decodeBatchFrame(frame(flags, 0x0101, [0xaa])),
      null,
      `a flag this client does not know: ${flags}`,
    );
  }
  assert.equal(
    decodeBatchFrame(frame(0x01, 0, [0xaa])),
    null,
    "an empty rectangle",
  );
  assert.equal(
    decodeBatchFrame(frame(0x01, 0x0101, [])),
    null,
    "a unit of nothing",
  );
  // Cut short of its unit, and of its header.
  const lone = frame(0x01, 0x0101, [0xaa, 0xbb]).slice(0, 8 + 15 + 2);
  const one = new Uint8Array(lone);
  one[2] = 0x01;
  assert.equal(decodeBatchFrame(one.buffer)?.length, 1);
  assert.equal(decodeBatchFrame(one.slice(0, one.byteLength - 1).buffer), null);
  assert.equal(decodeBatchFrame(one.slice(0, 8 + 10).buffer), null);
});

test("a video record's flags say a keyframe, and which strip of its picture it is", () => {
  // Transcribed from `VideoUnit::write_record` in src/protocol.rs: op 0x03, flags,
  // the size and the length little-endian, the unit.
  const record = (flags: number) =>
    decodeBatchFrame(
      new Uint8Array([
        0x02,
        0x00,
        0x01,
        0x00,
        0x05,
        0x00,
        0x00,
        0x00,
        0x03,
        flags,
        0xa0,
        0x05,
        0x84,
        0x03,
        0x01,
        0x00,
        0x00,
        0x00,
        0xee,
      ]).buffer,
    )?.[0];
  assert.deepEqual(record(0x01), {
    kind: "video",
    w: 1440,
    h: 900,
    keyframe: true,
    data: new Uint8Array([0xee]),
  });
  const strip = (flags: number) => {
    const unit = record(flags);
    return unit?.kind === "video" ? [unit.keyframe, unit.strip] : unit;
  };
  assert.deepEqual(strip(0x13), [true, { index: 0, ends: true }]);
  assert.deepEqual(strip(0x0e), [false, { index: 3, ends: false }]);
  assert.deepEqual(strip(0x1a), [false, { index: 2, ends: true }]);
  // A strip's number or frame on a unit that is no strip, and a bit not known.
  assert.equal(record(0x04), undefined);
  assert.equal(record(0x10), undefined);
  assert.equal(record(0x22), undefined);
});

test("an ordinary click run is passed through as the browser counted it", () => {
  assert.equal(clickCount(1), 1);
  assert.equal(clickCount(2), 2);
  assert.equal(clickCount(3), 3);
});

test("a detail the wire cannot carry is bounded rather than dropped", () => {
  // One byte on the wire, and a held-down finger on a trackpad can run the
  // count past it. Anything over two is a triple-click to every app that cares.
  assert.equal(clickCount(255), 255);
  assert.equal(clickCount(256), 255);
  assert.equal(clickCount(9999), 255);
});

test("a detail of nothing is still one click", () => {
  // Programmatic and synthesized events arrive with a detail of 0, which would
  // otherwise inject a click state of 0 — a press macOS counts as no click.
  assert.equal(clickCount(0), 1);
  assert.equal(clickCount(-1), 1);
  assert.equal(clickCount(Number.NaN), 1);
  assert.equal(clickCount(1.7), 1);
});

test("the side buttons of a five-button mouse are named, not dropped", () => {
  // DOM 3 and 4 are back and forward, and macOS numbers them the same way.
  assert.equal(mouseButtonFromEvent(0), "left");
  assert.equal(mouseButtonFromEvent(1), "middle");
  assert.equal(mouseButtonFromEvent(2), "right");
  assert.equal(mouseButtonFromEvent(3), "back");
  assert.equal(mouseButtonFromEvent(4), "forward");
  // Past forward nothing has an agreed meaning on any platform.
  assert.equal(mouseButtonFromEvent(5), null);
});

test("a button's held bit follows MouseEvent.buttons, not MouseEvent.button", () => {
  assert.equal(mouseButtonBit("left"), 1);
  assert.equal(mouseButtonBit("right"), 2);
  assert.equal(mouseButtonBit("middle"), 4);
  assert.equal(mouseButtonBit("back"), 8);
  assert.equal(mouseButtonBit("forward"), 16);
});

test("a wheel delta says which unit it is in", () => {
  // The remote cannot tell a three-pixel trackpad glide from a three-line
  // notch, and guessing lines is what made trackpad scrolling jump.
  assert.equal(wheelUnitFromEvent(0), "pixel");
  assert.equal(wheelUnitFromEvent(1), "line");
  assert.equal(wheelUnitFromEvent(2), "page");
  // Anything else is what every browser on macOS actually sends.
  assert.equal(wheelUnitFromEvent(7), "pixel");
});

test("a mouse wheel reported in pixels is sent as its notches", () => {
  // Chromium: a notch is 100 pixels and a legacy delta of 120.
  assert.deepEqual(
    wheelFromEvent({
      deltaX: 0,
      deltaY: 100,
      deltaMode: 0,
      wheelDeltaX: 0,
      wheelDeltaY: -120,
    }),
    { dx: 0, dy: 1, unit: "notch" },
  );
  assert.deepEqual(
    wheelFromEvent({
      deltaX: 0,
      deltaY: -200,
      deltaMode: 0,
      wheelDeltaX: 0,
      wheelDeltaY: 240,
    }),
    { dx: 0, dy: -2, unit: "notch" },
  );
  // Shift turns the wheel sideways.
  assert.deepEqual(
    wheelFromEvent({
      deltaX: 100,
      deltaY: 0,
      deltaMode: 0,
      wheelDeltaX: -120,
      wheelDeltaY: 0,
    }),
    { dx: 1, dy: 0, unit: "notch" },
  );
});

test("a glide stays the distance it is", () => {
  assert.deepEqual(
    wheelFromEvent({
      deltaX: 1,
      deltaY: 7,
      deltaMode: 0,
      wheelDeltaX: -3,
      wheelDeltaY: -21,
    }),
    { dx: 1, dy: 7, unit: "pixel" },
  );
  // A trackpad's legacy delta is three times its pixels, so 40 of them land
  // on a notch's 120 without being one.
  assert.deepEqual(
    wheelFromEvent({
      deltaX: 0,
      deltaY: 40,
      deltaMode: 0,
      wheelDeltaX: 0,
      wheelDeltaY: -120,
    }),
    { dx: 0, dy: 40, unit: "pixel" },
  );
  // The legacy delta is a whole number, so a fraction of a pixel either side
  // of 40 lands on 120 as well.
  for (const deltaY of [39.7, 40.1, 40.3]) {
    assert.deepEqual(
      wheelFromEvent({
        deltaX: 0,
        deltaY,
        deltaMode: 0,
        wheelDeltaX: 0,
        wheelDeltaY: -120,
      }),
      { dx: 0, dy: deltaY, unit: "pixel" },
    );
  }
  // One axis off a notch is no wheel.
  assert.deepEqual(
    wheelFromEvent({
      deltaX: 5,
      deltaY: 100,
      deltaMode: 0,
      wheelDeltaX: -6,
      wheelDeltaY: -120,
    }),
    { dx: 5, dy: 100, unit: "pixel" },
  );
  // Nothing to go by, and a wheel already in its own unit.
  assert.deepEqual(wheelFromEvent({ deltaX: 0, deltaY: 100, deltaMode: 0 }), {
    dx: 0,
    dy: 100,
    unit: "pixel",
  });
  assert.deepEqual(
    wheelFromEvent({
      deltaX: 0,
      deltaY: 3,
      deltaMode: 1,
      wheelDeltaX: 0,
      wheelDeltaY: -120,
    }),
    { dx: 0, dy: 3, unit: "line" },
  );
});

// The layout mirrors `audio` in src/protocol.rs, whose `frame` and `gap` write
// these bytes.
test("an audio frame is its packets, and a gap is a flagged frame of none", () => {
  const frame = new Uint8Array([0x03, 0, 2, 0, 1, 0, 9, 2, 0, 7, 8]);
  const decoded = decodeAudioFrame(frame.buffer);
  assert.equal(decoded?.gap, false);
  assert.deepEqual(
    decoded?.packets.map((packet) => Array.from(packet)),
    [[9], [7, 8]],
  );

  const gap = decodeAudioFrame(new Uint8Array([0x03, 1, 0, 0]).buffer);
  assert.deepEqual(gap, { gap: true, packets: [] });
});

test("an audio frame with a flag this client does not know is dropped whole", () => {
  assert.equal(decodeAudioFrame(new Uint8Array([0x03, 2, 0, 0]).buffer), null);
  assert.equal(decodeAudioFrame(new Uint8Array([0x03, 3, 0, 0]).buffer), null);
  // A count that the packets present do not match is a truncated frame.
  assert.equal(
    decodeAudioFrame(new Uint8Array([0x03, 0, 2, 0, 1, 0, 9]).buffer),
    null,
  );
});
