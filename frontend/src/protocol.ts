// Wire protocol shared (in shape) with the Rust backend `src/protocol.rs`.
//
// Browser -> server: input events as JSON text frames on `/ws`.
// Server -> browser: screen batches on `/ws` and audio frames on `/ws/audio`,
// with their kind in the first byte; control messages (resize/error on the first
// socket, the audio format on the second) are tagged JSON text frames.

// "back" and "forward" are the side buttons of a five-button mouse. No engine
// acts on them today — RDP and VNC drop them for want of anywhere to put them.
export type MouseButton = "left" | "middle" | "right" | "back" | "forward";

// What a wheel delta is measured in: the DOM's deltaMode, by name. Carried
// because only the browser knows — the remote cannot tell a three-pixel trackpad
// glide from a three-line wheel notch, and treating every delta as lines is what
// made trackpad scrolling jump.
export type WheelUnit = "pixel" | "line" | "page";

// What a touch contact did: MS-RDPEI's four contact transitions, which are
// also exactly the DOM's four touch events. `cancel` is lost rather than
// lifted — the guest forgets the gesture where an `up` would have been a tap.
export type TouchPhase = "down" | "move" | "up" | "cancel";

// Browser -> server: input events captured over the remote canvas, plus
// viewport reports (the desired remote desktop size, in points: the room the
// browser has, in CSS pixels — the engine renders them at its own density, so
// the report needs no scale this browser may not have been told yet. Engines
// that support dynamic resize act on them, the rest ignore them).
export type ClientMsg =
  | { type: "mouseMove"; x: number; y: number }
  // `clicks` is the browser's own click count for the press (MouseEvent.detail):
  // 1 for a single click, 2 for the second of a double. It still rides the wire,
  // but neither current engine consumes it — RDP and VNC carry button state alone
  // and let the guest count.
  | {
      type: "mouseButton";
      button: MouseButton;
      pressed: boolean;
      clicks: number;
    }
  | { type: "wheel"; dx: number; dy: number; unit: WheelUnit }
  // `caps` carries KeyboardEvent.getModifierState("CapsLock") so the backend
  // knows the lock state authoritatively (it can't otherwise tell CapsLock is
  // already on at connect time). Synthetic sends without a real event pass
  // false — they express case through an explicit Shift code instead.
  | { type: "key"; code: string; pressed: boolean; caps: boolean }
  // Requested desktop size in points (CSS pixels). Engines apply their own policy.
  | { type: "viewport"; w: number; h: number }
  // Restore the target-defined default size; distinct from sending no request.
  | { type: "defaultSize" }
  // The screen this browser window is on: its full resolution in CSS pixels
  // (`screen.width`/`screen.height`) and its density in hundredths
  // (`devicePixelRatio * 100`, so 100 for a 1x screen and 200 for a Retina
  // one). Sent on connect and again whenever the window lands on a different
  // screen. Mid-session only the density is acted on (RDP density matching,
  // a High Performance Mac re-rendering at the new density); the size fields
  // matter at session-open, where `connect` carries the same shape.
  // `fit` marks the pinch-zoom client (CAN_PINCH_ZOOM), which presents the
  // desktop scaled to fit rather than at 100%: its screen is then no opening
  // size, and a target with no pinned size opens at the gateway's default
  // for it. It does not affect canvas layout.
  | { type: "hostDisplay"; w: number; h: number; scale: number; fit: boolean }
  // Session control (handled by the server's session slot, not an engine):
  // pick a target from the post-login picker, or tear the session down and
  // switch back to it. The connect names this window's screen so a target
  // with no pinned config size opens at its full resolution — by the time a
  // hostDisplay message could arrive, the opening size has already been
  // asked of the remote.
  | {
      type: "connect";
      target: string;
      display: { w: number; h: number; scale: number; fit: boolean };
    }
  | { type: "disconnect" }
  // Clipboard bridge. The backend owns the clipboard data: "clipboard" puts
  // text on the remote's clipboard, "clipboardRequest" asks for the remote's
  // current text and is answered with a `clipboard` control message. Both are
  // sent either by the floating menu's Clipboard panel or by the automatic
  // sync in useRemoteDesktop, which pushes the local OS clipboard on focus
  // where the browser permits reading it. Nothing is retained here.
  | { type: "clipboard"; text: string }
  | { type: "clipboardRequest" }
  // Select an opaque id from the latest `displays` message.
  | { type: "selectDisplay"; id: number }
  // One touch contact's transition in framebuffer pixels: the touchscreen
  // mode, where fingers reach the remote as the contacts they are and the
  // guest recognises the gestures. `id` names the finger from its down to its
  // up or cancel — a small slot assigned by touchPassthrough.ts, not the DOM's
  // identifier. Acted on by the RDP engine once the host has opened the touch
  // channel (`touchReady`); dropped everywhere else.
  | { type: "touch"; id: number; phase: TouchPhase; x: number; y: number }
  // Re-announce the desktop size and repaint everything. A recovery command for
  // a canvas that has gone wrong; the
  // browser has no button for it, since there is a reload right there.
  | { type: "refresh" }
  // There is no audio message. Subscribing is opening `/ws/audio`, and stopping is
  // closing it — see useRemoteDesktop's `setAudio`. The UI click that does it also
  // authorizes playback, which is why it has to be a click either way.
  // The paint worker finished this batch after its ordered parse/decode/draw
  // pass. The sequence came from the batch header; the timings make the
  // browser backlog visible to the gateway instead of stopping at WebSocket
  // delivery, which has no receive-side backpressure signal.
  | {
      type: "paintAck";
      sequence: number;
      queuedMs: number;
      drawMs: number;
    }
  // The camera socket's opening message — its only inbound text — announcing
  // the H.264 the browser's encoder will produce. Sending it is what plugs the
  // virtual device into the remote; never sent on the session socket. The rate
  // is rational because both ends speak one (29.97 is 30000/1001).
  | {
      type: "cameraFormat";
      width: number;
      height: number;
      fpsNumerator: number;
      fpsDenominator: number;
    };

// Ceiling on one clipboard transfer, mirroring MAX_CLIPBOARD_BYTES in
// src/protocol.rs. The backend refuses anything over it in either direction;
// checking here too is what lets the panel say so before the round trip.
export const MAX_CLIPBOARD_BYTES = 524_288;

export interface ClipboardSnapshot {
  text: string;
  // Unix epoch milliseconds when remotex observed the remote clipboard
  // change. Null is honest for clipboard content that predates this session.
  changedAtMs: number | null;
  // Set when the remote's clipboard was refused for exceeding
  // MAX_CLIPBOARD_BYTES, to the size it actually is. `text` is empty then, and
  // this is what keeps that apart from a remote that has copied nothing —
  // truncating instead would have arrived looking like the whole clipboard.
  oversizedBytes: number | null;
}

export interface RemoteClipboard extends ClipboardSnapshot {
  // Ticks on every reply/push so an identical Fetch is still observable.
  seq: number;
}

// One of the remote's displays, as the picker lists it. The strings are built
// by the remote end and shown verbatim: the Mac knows how its own displays are
// named and numbered, and saying it once keeps every part of the panel consistent.
export interface DisplayInfo {
  // Opaque here — whatever goes back in a "selectDisplay".
  id: number;
  // Short enough for a button: "Display 2", or "Virtual display".
  label: string;
  // The line under it: "1600×1000 at 2x".
  detail: string;
  main: boolean;
  // A display the remote made for this purpose rather than one of its screens.
  virtual: boolean;
}

// A rectangle in whole pixels or points, origin at the top left.
export interface MosaicRect {
  x: number;
  y: number;
  w: number;
  h: number;
}

// One screen of a composed view: its pixels in the framebuffer, and where it
// belongs in the remote's arrangement, in points.
export interface MosaicRegion {
  pixels: MosaicRect;
  points: MosaicRect;
}

// Server -> browser text frames: everything but the video stream. `resize`/`error`
// come from the engine; `picker`/`connected` are the session-slot status the
// server sends so the browser knows which post-login state it is in.
export type ControlMsg =
  // `w`/`h` are framebuffer pixels; `scale` is how many of them the remote draws
  // per point of its *own* desktop (1 for VNC, RDP and a 1x Mac, 2 for a Retina
  // one). The canvas bitmap remains `w` by `h`, while its CSS box is
  // `w / scale` by `h / scale`, preserving every source pixel without changing
  // the remote desktop's logical size.
  | {
      type: "resize";
      w: number;
      h: number;
      scale: number;
    }
  // The remote pointer shape, sent only by engines whose server hands the
  // cursor over instead of drawing it into the framebuffer (the VNC Cursor
  // pseudo-encoding). Receiving one at all means the browser owns pointer
  // rendering from then on; `image` is a base64 PNG, null when the remote hid
  // the pointer. `hx`/`hy` are the hotspot within the image. `pointSized`
  // names the image's unit: true for Apple's density-independent point-sized
  // pixmaps, which are drawn against the desktop's points; false for RDP/RFB
  // cursors cut from the desktop's own pixels, drawn against the framebuffer.
  | {
      type: "cursor";
      image: string | null;
      w: number;
      h: number;
      hx: number;
      hy: number;
      pointSized: boolean;
    }
  | { type: "error"; message: string }
  | { type: "picker" }
  // `resize` means this window drives the remote's size, continuously and on
  // every engine alike — the operator's one switch, with no client-side mode.
  // True is auto-follow (and the mobile one-shot); false is a session whose
  // size was settled at open.
  // `protocol` ("rdp"/"vnc") is carried for the status line. `clipboard` is
  // whether this target opted into the clipboard bridge. `audio` advertises
  // capability, not current activity.
  | {
      type: "connected";
      name: string;
      protocol: string;
      // The target's `subtype` where it has one — `ard` or
      // `ard-high-performance` — and null for plain RDP and plain VNC. Three
      // targets say `vnc` and only this tells them apart, which is what the
      // session card's Connection row is for: whether there is a display list,
      // whether resize is offered, and whether the path under it is the
      // reverse-engineered one. See connectionLabel.ts.
      subtype: string | null;
      resize: boolean;
      clipboard: boolean;
      audio: boolean;
      // Whether this target redirects the browser's camera to the remote.
      // Capability only, like `audio` — enabling is this client's move, made
      // afresh each session by opening /ws/camera, never persisted.
      camera: boolean;
      // Whether this target redirects the browser's microphone to the remote. The
      // camera's twin: enabled afresh each session by opening /ws/mic.
      microphone: boolean;
      // The render dial this session resolved to, in one line —
      // `video q90 4:4:4 · adaptive ≥20`. The *resolved plan* rather than the config
      // keys, which the reader may not have: defaults and the browser's own chroma
      // are already applied.
      render: string;
    }
  // How to play the audio frames that follow, sent once when audio is enabled and
  // always before the first packet — a decoder configured afterwards has already
  // thrown away the audio it was meant to decode.
  //
  // `codec` is `opus`, with the base64 `OpusHead` in `head` and `sampleRate` the
  // 48 kHz the gateway resampled to. `packetFrames` is the samples in one packet
  // at `sampleRate` — 960 — and is the one thing a client cannot derive for itself.
  | {
      type: "audioFormat";
      codec: string;
      sampleRate: number;
      channels: number;
      packetFrames: number;
      head: string;
    }
  // How to decode one video stream, sent before its first VIDEO record and again
  // whenever it changes — a decoder configured afterwards has already thrown away the
  // frame it was meant to decode. The video counterpart of `audioFormat`.
  //
  // One per session, plus one per resize and per repaint: the string carries a
  // size-derived level, and a browser that just attached has seen none.
  //
  // `decode` is the exact WebCodecs string to configure with: `vp09.00.40.08.01.06.06.06.00`.
  // Nothing here parses a bitstream to find it out; VP9 has no parameter sets to
  // parse.
  | { type: "videoFormat"; decode: string }
  // Whether the remote runs macOS, discovered by the engine as it connects.
  // The browser uses it to decide whether selected local Command shortcuts stay
  // Command or become remote Control.
  | { type: "remoteOs"; macos: boolean }
  // The host opened its touch channel (MS-RDPEI), so `touch` messages reach it
  // as real contacts from now on. Sent by the RDP engine shortly after connect
  // and again on every reattach; never for a host without the channel, which
  // is what hides the Touchscreen toggle there. Not part of `connected`: the
  // capability is the host's answer, not the target's profile.
  | { type: "touchReady" }
  // A High Performance Mac is being resized and the picture is not the
  // window's yet: true from the window's new size until the Mac's answering
  // layout has held still. The page covers the desktop meanwhile, as Apple's
  // client does, so a settling resize's intermediate modes are not shown.
  // Pushed by the gateway, which alone knows when the Mac has settled; the
  // page never infers it.
  | { type: "resizing"; active: boolean }
  // Whether the desktop the `resize` before this describes arrives as the remote's
  // own rectangles (TILE records) rather than as video, because it is past what a
  // video stream encodes. Sent after every `resize` of a source that can do that,
  // and never by one that cannot — whose pictures are always video.
  | { type: "tiling"; active: boolean }
  // The remote's displays and which one is being shared, pushed whenever either
  // changes. The browser holds no display state of its own: the checkmark
  // follows `active`, so a selection the remote refused leaves the panel
  // showing what is really on screen. An engine that cannot offer a choice
  // never sends this, and the FAB then has no Display section at all.
  | { type: "displays"; active: number; displays: DisplayInfo[] }
  // How the next framebuffers are presented when no one density does it: a
  // Mac's combined view of screens at different densities. Each region names
  // a screen's pixels in the framebuffer and its place in points; the page
  // composes them at this display's density (mosaic.ts). Sent ahead of the
  // `resize` it describes; `resize` says one follows, and the regions wait
  // for it rather than recompose the framebuffer still on screen. Empty ends
  // it.
  | { type: "mosaic"; regions: MosaicRegion[]; resize: boolean }
  // The remote's clipboard text: either the reply to a "clipboardRequest" or
  // an unprompted push when the remote's clipboard changed. Requested replies
  // populate the panel without silently copying; pushes retain automatic sync.
  | ({ type: "clipboard"; requested: boolean } & ClipboardSnapshot)
  // Camera-socket traffic only, the remote's streaming decisions: an
  // application on the remote opened the camera, so encode and send from a
  // keyframe on (`cameraStart`, whose format is the confirmation of what this
  // client announced — the gateway advertises exactly one media type); it
  // closed the camera (`cameraStop`); or samples were dropped and the next
  // frame must be a keyframe (`cameraKeyframe`).
  | {
      type: "cameraStart";
      width: number;
      height: number;
      fpsNumerator: number;
      fpsDenominator: number;
    }
  | { type: "cameraStop" }
  | { type: "cameraKeyframe" }
  // Mic-socket traffic only: an application on the remote started recording
  // from the microphone (`micOpen`), so encode and send, or stopped (`micClose`).
  | { type: "micOpen" }
  | { type: "micClose" };

// One video access unit of the desktop's stream.
//
// One link in a chain, where losing any link decodes wrongly until the next
// keyframe — so none may be dropped, reordered, or decoded twice.
//
// `(w, h)` is the true desktop size. The decoded picture may be a pixel wider or
// taller, because the encoder is held to even sides and an odd desktop does not have
// them: draw the top-left w×h of it. A size that differs from the last unit's means
// the stream started over on a differently sized picture.
//
// `keyframe` comes from the record's flags byte, and so from the encoder rather than
// from a parse of what it produced. It is on the wire because VP9 carries no parameter
// sets: there is nothing in a VP9 payload to read it out of. `videoFormat` says how to
// configure the decoder, and always arrives first. See `VideoUnit` in src/protocol.rs
// for the whole contract.
export interface VideoMsg {
  kind: "video";
  w: number;
  h: number;
  keyframe: boolean;
  data: Uint8Array;
}

// One rectangle of the remote's framebuffer, as the remote sent it: a PNG to draw
// at (x, y) over what the canvas holds. It depends on nothing before it. See `Tile`
// in src/protocol.rs for the contract.
export interface TileMsg {
  kind: "tile";
  x: number;
  y: number;
  w: number;
  h: number;
  data: Uint8Array;
}

export type BatchRecord = VideoMsg | TileMsg;

const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const AUDIO_FRAME_KIND = 0x03;
const AUDIO_HEADER_LEN = 4;
const AUDIO_PACKET_HEADER_LEN = 2;
const CAMERA_FRAME_KIND = 0x04;
const CAMERA_KEYFRAME = 0x01;
const MIC_FRAME_KIND = 0x05;
const OP_TILE = 0x01;
const TILE_HEADER_LEN = 13;
const OP_VIDEO = 0x03;
const VIDEO_HEADER_LEN = 10;
// A VIDEO record's only flag: a decoder that has seen nothing before it can start here.
// Any other bit means a gateway newer than this client, and the record is dropped rather
// than guessed at — the same strictness the batch's own flags byte gets.
const VIDEO_KEYFRAME = 0x01;

// Parse a binary batch frame into its records. Layout (little-endian,
// matching `batch` in `src/protocol.rs`):
//
//   offset 0: u8  frame kind, always 0x02 (batch)
//   offset 1: u8  flags, always 0
//   offset 2: u16 record count
//   offset 4: u32 sequence, increasing per attachment
//   offset 8: records, back to back
//
//   TILE  (op 0x01):     u16 x | u16 y | u16 w | u16 h | u32 len | png[len]
//   VIDEO (op 0x03):     u8 flags | u16 w | u16 h | u32 len | payload[len]
//
// Returns null for anything malformed or unknown, so callers can drop a bad
// frame whole rather than paint half of it. A truncated frame is *detectable*
// only because the header carries a record count — without it, a short read
// would look like a complete but smaller batch.
export function decodeBatchFrame(buf: ArrayBuffer): BatchRecord[] | null {
  if (batchFrameSequence(buf) === null) {
    return null;
  }
  const view = new DataView(buf);
  const count = view.getUint16(2, true);
  const records: BatchRecord[] = [];
  let at = BATCH_HEADER_LEN;
  while (at < buf.byteLength) {
    const parsed =
      view.getUint8(at) === OP_TILE
        ? decodeTile(view, buf, at)
        : decodeVideo(view, buf, at);
    if (!parsed) {
      return null;
    }
    records.push(parsed.record);
    at = parsed.next;
  }
  return records.length === count ? records : null;
}

/**
 * Read the sequence that lets the paint worker acknowledge this batch after
 * it has finished. Header validation lives here so the main thread never posts
 * an acknowledgment-capable command for a non-batch, reserved future flags,
 * or sequence zero (real attachment sequences start at one).
 */
export function batchFrameSequence(buf: ArrayBuffer): number | null {
  if (buf.byteLength < BATCH_HEADER_LEN) {
    return null;
  }
  const view = new DataView(buf);
  if (view.getUint8(0) !== BATCH_FRAME_KIND || view.getUint8(1) !== 0) {
    return null;
  }
  const sequence = view.getUint32(4, true);
  return sequence === 0 ? null : sequence;
}

function decodeVideo(
  view: DataView,
  buf: ArrayBuffer,
  at: number,
): { record: VideoMsg; next: number } | null {
  if (
    view.getUint8(at) !== OP_VIDEO ||
    at + VIDEO_HEADER_LEN > buf.byteLength
  ) {
    return null;
  }
  const flags = view.getUint8(at + 1);
  if ((flags & ~VIDEO_KEYFRAME) !== 0) {
    return null;
  }
  const len = view.getUint32(at + 6, true);
  const start = at + VIDEO_HEADER_LEN;
  if (start + len > buf.byteLength) {
    return null;
  }
  return {
    record: {
      kind: "video",
      w: view.getUint16(at + 2, true),
      h: view.getUint16(at + 4, true),
      keyframe: (flags & VIDEO_KEYFRAME) !== 0,
      data: new Uint8Array(buf, start, len),
    },
    next: start + len,
  };
}

// A tile of no area or no payload is malformed: nothing could be drawn for it.
function decodeTile(
  view: DataView,
  buf: ArrayBuffer,
  at: number,
): { record: TileMsg; next: number } | null {
  if (at + TILE_HEADER_LEN > buf.byteLength) {
    return null;
  }
  const w = view.getUint16(at + 5, true);
  const h = view.getUint16(at + 7, true);
  const len = view.getUint32(at + 9, true);
  const start = at + TILE_HEADER_LEN;
  if (w === 0 || h === 0 || len === 0 || start + len > buf.byteLength) {
    return null;
  }
  return {
    record: {
      kind: "tile",
      x: view.getUint16(at + 1, true),
      y: view.getUint16(at + 3, true),
      w,
      h,
      data: new Uint8Array(buf, start, len),
    },
    next: start + len,
  };
}

// Build one camera frame: the only binary this client *sends*. Layout (matching
// `camera` in `src/protocol.rs`):
//
//   offset 0: u8 frame kind, always 0x04 (camera sample)
//   offset 1: u8 flags — bit 0 set on a keyframe
//   offset 2: one encoded H.264 access unit, to the end of the frame
//
// One access unit per WebSocket frame — the unit of transfer is the unit of
// decode — with the keyframe bit carried so the gateway can drop and recover a
// stream without parsing H.264. An empty unit is null rather than a frame: the
// gateway's parser rejects a payload-less frame as malformed, so building one
// here would only spend a send on bytes the far end drops.
export function encodeCameraFrame(
  unit: Uint8Array,
  keyframe: boolean,
): Uint8Array<ArrayBuffer> | null {
  if (unit.byteLength === 0) {
    return null;
  }
  const frame = new Uint8Array(2 + unit.byteLength);
  frame[0] = CAMERA_FRAME_KIND;
  frame[1] = keyframe ? CAMERA_KEYFRAME : 0;
  frame.set(unit, 2);
  return frame;
}

// Build one microphone frame (matching `mic` in `src/protocol.rs`): the kind
// byte 0x05, then one Opus packet to the end of the frame. An empty packet is
// null, which the gateway's parser would reject anyway.
export function encodeMicFrame(
  packet: Uint8Array,
): Uint8Array<ArrayBuffer> | null {
  if (packet.byteLength === 0) {
    return null;
  }
  const frame = new Uint8Array(1 + packet.byteLength);
  frame[0] = MIC_FRAME_KIND;
  frame.set(packet, 1);
  return frame;
}

// Read the binary kind before choosing the independent batch or audio parser.
export function binaryFrameKind(buf: ArrayBuffer): "batch" | "audio" | null {
  if (buf.byteLength < 1) {
    return null;
  }
  switch (new DataView(buf).getUint8(0)) {
    case BATCH_FRAME_KIND:
      return "batch";
    case AUDIO_FRAME_KIND:
      return "audio";
    default:
      return null;
  }
}

// Parse an audio frame into its Opus packets. Layout (little-endian, matching
// `audio` in `src/protocol.rs`):
//
//   offset 0: u8  frame kind, always 0x03 (audio)
//   offset 1: u8  flags, always 0
//   offset 2: u16 packet count
//   offset 4: packets, each u16 length | length bytes
//
// Lengths because an Opus packet does not carry its own size and one frame holds
// nine or ten of them; a count because a truncated frame would otherwise look like a
// complete shorter one. Returns null for anything malformed, so a bad frame is
// dropped whole rather than decoded halfway — a decoder fed a partial packet does not
// merely skip it, it can be left unable to decode what follows.
export function decodeAudioFrame(buf: ArrayBuffer): Uint8Array[] | null {
  if (buf.byteLength < AUDIO_HEADER_LEN) {
    return null;
  }
  const view = new DataView(buf);
  if (view.getUint8(0) !== AUDIO_FRAME_KIND || view.getUint8(1) !== 0) {
    return null;
  }
  const count = view.getUint16(2, true);
  const packets: Uint8Array[] = [];
  let at = AUDIO_HEADER_LEN;
  while (at < buf.byteLength) {
    if (at + AUDIO_PACKET_HEADER_LEN > buf.byteLength) {
      return null;
    }
    const len = view.getUint16(at, true);
    const start = at + AUDIO_PACKET_HEADER_LEN;
    if (start + len > buf.byteLength) {
      return null;
    }
    packets.push(new Uint8Array(buf, start, len));
    at = start + len;
  }
  return packets.length === count ? packets : null;
}

// The click count to report for a mouse event. `detail` is what the browser
// decided, applying the platform's own double-click policy, so it is taken as
// given — only bounded, since the wire carries a single byte and a programmatic
// event can arrive with a detail of 0.
export function clickCount(detail: number): number {
  return Math.min(255, Math.max(1, Math.trunc(detail) || 1));
}

// The unit a WheelEvent's deltas are in. WheelEvent.deltaMode is 0/1/2 for
// pixels/lines/pages; an unknown value reads as pixels, which is what every
// browser on macOS reports and so the safest thing to be wrong about.
export function wheelUnitFromEvent(deltaMode: number): WheelUnit {
  switch (deltaMode) {
    case 1:
      return "line";
    case 2:
      return "page";
    default:
      return "pixel";
  }
}

// The bit a button holds in DOM `MouseEvent.buttons`, which numbers them
// differently from `MouseEvent.button`: right is 2 and middle 4 there.
export function mouseButtonBit(button: MouseButton): number {
  switch (button) {
    case "left":
      return 1;
    case "right":
      return 2;
    case "middle":
      return 4;
    case "back":
      return 8;
    case "forward":
      return 16;
  }
}

// Map DOM MouseEvent.button to the protocol button name. 3 and 4 are the back
// and forward buttons; anything past them has no agreed meaning on any platform.
export function mouseButtonFromEvent(button: number): MouseButton | null {
  switch (button) {
    case 0:
      return "left";
    case 1:
      return "middle";
    case 2:
      return "right";
    case 3:
      return "back";
    case 4:
      return "forward";
    default:
      return null;
  }
}
