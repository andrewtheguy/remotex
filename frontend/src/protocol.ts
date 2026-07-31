// Wire protocol shared (in shape) with the Rust backend `src/protocol.rs`.
//
// Browser -> server: input events as JSON text frames.
// Server -> browser: screen tiles and the remote's audio as binary frames, told
// apart by their first byte (binaryFrameKind below); rare control messages
// (resize/error, the audio format) as JSON text frames with a `type` tag.

export type MouseButton = "left" | "middle" | "right";

// Browser -> server: input events captured over the remote canvas, plus
// viewport reports (the desired remote desktop size, in the *remote's* pixels:
// the room the browser has times the density the remote draws at — engines that
// support dynamic resize act on them, the rest ignore them).
export type ClientMsg =
  | { type: "mouseMove"; x: number; y: number }
  // `clicks` is the browser's own click count for the press (MouseEvent.detail):
  // 1 for a single click, 2 for the second of a double. The rxa engine injects it
  // as the macOS event's click state, which is the only thing that makes a
  // double-click a double-click — the remote cannot work it out from arrival
  // times. RDP and VNC have nowhere to put it and let the guest count instead.
  | {
      type: "mouseButton";
      button: MouseButton;
      pressed: boolean;
      clicks: number;
    }
  | { type: "wheel"; dx: number; dy: number }
  // `caps` carries KeyboardEvent.getModifierState("CapsLock") so the backend
  // knows the lock state authoritatively (it can't otherwise tell CapsLock is
  // already on at connect time). Synthetic sends without a real event pass
  // false — they express case through an explicit Shift code instead.
  | { type: "key"; code: string; pressed: boolean; caps: boolean }
  // Requested framebuffer size in remote pixels. Engines apply their own policy.
  | { type: "viewport"; w: number; h: number }
  // Restore the target-defined default size; distinct from sending no request.
  | { type: "defaultSize" }
  // The density of the screen this browser window is on, in hundredths:
  // `devicePixelRatio * 100`, so 100 for a 1x screen and 200 for a Retina one.
  // Sent on connect and again whenever the window moves to a screen of a
  // different density.
  //
  // Only an owned RXA display acts on this; it does not affect canvas layout.
  | { type: "hostScale"; scale: number }
  // Session control (handled by the server's session slot, not an engine):
  // pick a target from the post-login picker, or tear the session down and
  // switch back to it.
  // `force` takes the *remote's* session slot from whoever holds it, answering a
  // `remoteBusy`. Not about this gateway's slot, which is claimed over HTTP before
  // the socket exists — only an rxa target has a session of its own to contend
  // for, and the other engines ignore it.
  | { type: "connect"; target: string; force: boolean }
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
  // "I lost the tiles you told me to remember." Sent when a cached tile will not
  // decode, or when a reference names a slot this client does not hold. The
  // server empties its slot table and repaints; deliberately not "refresh",
  // which is routed to the engine and would leave the table intact — the repaint
  // would come back as the same references and miss again.
  | { type: "cacheReset" }
  // Start or stop audio on this attachment. The UI click also authorizes playback.
  | { type: "audio"; enabled: boolean };

// Ceiling on one clipboard transfer, mirroring MAX_CLIPBOARD_BYTES in
// src/protocol.rs. The backend refuses anything over it in either direction;
// checking here too is what lets the panel say so before the round trip.
export const MAX_CLIPBOARD_BYTES = 65_536;

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
// named and numbered, and saying it once keeps this panel and the native
// viewer's Display menu reading the same.
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

// Server -> browser text frames: everything but screen tiles. `resize`/`error`
// come from the engine; `picker`/`connected` are the session-slot status the
// server sends so the browser knows which post-login state it is in.
export type ControlMsg =
  // `w`/`h` are framebuffer pixels; `scale` is how many of them the remote draws
  // per point of its *own* desktop (1 for VNC, RDP and a 1x Mac, 2 for a Retina
  // one). The canvas is presented at `w / scale` CSS pixels, so the desktop keeps
  // its physical size whatever the density of the screen showing it.
  | { type: "resize"; w: number; h: number; scale: number }
  // The remote pointer shape, sent only by engines whose server hands the
  // cursor over instead of drawing it into the framebuffer (the VNC Cursor
  // pseudo-encoding). Receiving one at all means the browser owns pointer
  // rendering from then on; `image` is a base64 PNG, null when the remote hid
  // the pointer. `hx`/`hy` are the hotspot within the image.
  | {
      type: "cursor";
      image: string | null;
      w: number;
      h: number;
      hx: number;
      hy: number;
    }
  | { type: "error"; message: string }
  // The remote refused the session because a different client holds it, and this
  // one may take it over. Only an rxa target sends it: the macOS agent serves one
  // session at a time and says who has it, where an RDP or VNC server arbitrates
  // its own sessions silently.
  //
  // Distinct from "error" because it is actionable — the picker offers a takeover
  // rather than only reporting — and distinct from the ConnectionStatus "busy",
  // which is about *this gateway's* slot being held by another browser. Both can
  // happen at once, and they need different buttons: one takes the gateway, the
  // other takes the Mac at the far end of it.
  // `takenOver` separates the two ways this arrives, which are opposite
  // experiences: false is "you asked for a target somebody else has, and were
  // refused"; true is "you *had* it and somebody took it". Only the remote's
  // session went in that second case — the login, the gateway slot and this socket
  // are all still ours — which is why both land on the target list.
  | {
      type: "remoteBusy";
      holder: string;
      heldSecs: number;
      takenOver: boolean;
    }
  | { type: "picker" }
  // `resize` is the operator's permission to change the remote's size at all, and
  // it is all this says: whether the size then follows the window or moves only
  // when asked is the client's own choice, the same choice on every protocol (see
  // useRemoteDesktop's `autoResize`). `protocol` ("rdp"/"vnc"/"rxa") is here
  // because rxa's permission is only half-settled by `resize` — the display being
  // shared also has to be one the agent made, which arrives later in `displays`,
  // so an rxa session starts denied rather than waiting to be corrected.
  // `clipboard` is whether this target opted into the clipboard bridge.
  // `audio` advertises capability, not current activity.
  | {
      type: "connected";
      name: string;
      protocol: string;
      resize: boolean;
      clipboard: boolean;
      audio: boolean;
    }
  // How to decode the audio frames that follow, sent once when audio is enabled and
  // always before the first packet — a decoder configured afterwards has already
  // thrown away the audio it was meant to decode. `head` is base64 `OpusHead`,
  // which carries the channel count and the encoder's pre-skip.
  | {
      type: "audioFormat";
      codec: string;
      sampleRate: number;
      channels: number;
      head: string;
    }
  // Whether the remote runs macOS, discovered by the engine as it connects.
  // Only the native viewer acts on it, to decide whether a local Command
  // shortcut stays Command or becomes remote Control.
  | { type: "remoteOs"; macos: boolean }
  // The remote's displays and which one is being shared, pushed whenever either
  // changes. The browser holds no display state of its own: the checkmark
  // follows `active`, so a selection the remote refused leaves the panel
  // showing what is really on screen. An engine that cannot offer a choice
  // never sends this, and the FAB then has no Display section at all.
  | { type: "displays"; active: number; displays: DisplayInfo[] }
  // The remote's clipboard text: either the reply to a "clipboardRequest" or
  // an unprompted push when the remote's clipboard changed. Requested replies
  // populate the panel without silently copying; pushes retain automatic sync.
  | ({ type: "clipboard"; requested: boolean } & ClipboardSnapshot);

export interface TileMsg {
  x: number;
  y: number;
  w: number;
  h: number;
  // Where the server wants this remembered, or NO_SLOT for "do not".
  slot: number;
  // An encoded image stream, in `mime`.
  data: Uint8Array;
  // What `data` is, for the Blob handed to createImageBitmap. One codec: WebP
  // covers both the lossless screen content the gateway's engines send and the
  // lossy tiles the macOS agent classifies, so the choice never reaches the wire.
  mime: "image/webp";
}

// "Draw what you have in `slot` at (x, y)" — seven bytes instead of a payload.
export interface TileRefMsg {
  slot: number;
  x: number;
  y: number;
}

export type BatchRecord =
  | ({ kind: "tile" } & TileMsg)
  | ({ kind: "ref" } & TileRefMsg);

const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 4;
const AUDIO_FRAME_KIND = 0x03;
const AUDIO_HEADER_LEN = 4;
const AUDIO_PACKET_HEADER_LEN = 2;
const OP_TILE = 0x01;
const OP_TILE_REF = 0x02;
const TILE_HEADER_LEN = 16;
const TILE_REF_LEN = 7;
// The format byte, as the MIME type `createImageBitmap` needs for its Blob. One
// entry, and 3 rather than 1: `Tile::FORMAT_WEBP`. 1 and 2 meant PNG and JPEG, and
// leaving this map without them is what makes a gateway/SPA mismatch a rejected
// frame instead of WebP bytes handed to a PNG decoder.
const MIME_BY_FORMAT: Record<number, TileMsg["mime"] | undefined> = {
  3: "image/webp",
};
export const NO_SLOT = 0xffff;
// How many tiles the server may ask this client to remember. Part of the wire
// contract (`batch::SLOT_COUNT`), which is what makes the cache a fixed array
// rather than something a server could grow without limit.
export const SLOT_COUNT = 256;

// Parse a binary batch frame into its tile records. Layout (little-endian,
// matching `batch` in `src/protocol.rs`):
//
//   offset 0: u8  frame kind, always 0x02 (batch)
//   offset 1: u8  flags, always 0
//   offset 2: u16 record count
//   offset 4: records, back to back
//
//   TILE (op 0x01):     u8 format | u16 slot | u16 x | u16 y | u16 w | u16 h
//                       | u32 len | payload[len]
//   TILE_REF (op 0x02):  u16 slot | u16 x | u16 y
//
// Returns null for anything malformed or unknown, so callers can drop a bad
// frame whole rather than paint half of it. A truncated frame is *detectable*
// only because the header carries a record count — without it, a short read
// would look like a complete but smaller batch.
export function decodeBatchFrame(buf: ArrayBuffer): BatchRecord[] | null {
  if (buf.byteLength < BATCH_HEADER_LEN) {
    return null;
  }
  const view = new DataView(buf);
  if (view.getUint8(0) !== BATCH_FRAME_KIND || view.getUint8(1) !== 0) {
    return null;
  }
  const count = view.getUint16(2, true);
  const records: BatchRecord[] = [];
  let at = BATCH_HEADER_LEN;
  while (at < buf.byteLength) {
    const parsed =
      view.getUint8(at) === OP_TILE_REF
        ? decodeRef(view, buf.byteLength, at)
        : decodeTile(view, buf, at);
    if (!parsed) {
      return null;
    }
    records.push(parsed.record);
    at = parsed.next;
  }
  return records.length === count ? records : null;
}

function decodeRef(
  view: DataView,
  length: number,
  at: number,
): { record: BatchRecord; next: number } | null {
  if (at + TILE_REF_LEN > length) {
    return null;
  }
  const slot = view.getUint16(at + 1, true);
  if (slot >= SLOT_COUNT) {
    return null;
  }
  return {
    record: {
      kind: "ref",
      slot,
      x: view.getUint16(at + 3, true),
      y: view.getUint16(at + 5, true),
    },
    next: at + TILE_REF_LEN,
  };
}

function decodeTile(
  view: DataView,
  buf: ArrayBuffer,
  at: number,
): { record: BatchRecord; next: number } | null {
  if (view.getUint8(at) !== OP_TILE || at + TILE_HEADER_LEN > buf.byteLength) {
    return null;
  }
  const slot = view.getUint16(at + 2, true);
  if (slot !== NO_SLOT && slot >= SLOT_COUNT) {
    return null;
  }
  const mime = MIME_BY_FORMAT[view.getUint8(at + 1)];
  if (!mime) {
    return null;
  }
  const len = view.getUint32(at + 12, true);
  const start = at + TILE_HEADER_LEN;
  if (start + len > buf.byteLength) {
    return null;
  }
  return {
    record: {
      kind: "tile",
      slot,
      x: view.getUint16(at + 4, true),
      y: view.getUint16(at + 6, true),
      w: view.getUint16(at + 8, true),
      h: view.getUint16(at + 10, true),
      data: new Uint8Array(buf, start, len),
      mime,
    },
    next: start + len,
  };
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

// Map DOM MouseEvent.button (0/1/2) to the protocol button name.
export function mouseButtonFromEvent(button: number): MouseButton | null {
  switch (button) {
    case 0:
      return "left";
    case 1:
      return "middle";
    case 2:
      return "right";
    default:
      return null;
  }
}
