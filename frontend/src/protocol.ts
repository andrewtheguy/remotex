// Wire protocol shared (in shape) with the Rust backend `src/protocol.rs`.
//
// Browser -> server: input events as JSON text frames.
// Server -> browser: screen tiles as binary frames (decodeTileFrame below);
// rare control messages (resize/error) as JSON text frames with a `type` tag.

export type MouseButton = "left" | "middle" | "right";

// Browser -> server: input events captured over the remote canvas, plus
// viewport reports (the desired remote desktop size in device pixels —
// engines that support dynamic resize act on them, the rest ignore them).
export type ClientMsg =
  | { type: "mouseMove"; x: number; y: number }
  | { type: "mouseButton"; button: MouseButton; pressed: boolean }
  | { type: "wheel"; dx: number; dy: number }
  // `caps` carries KeyboardEvent.getModifierState("CapsLock") so the backend
  // knows the lock state authoritatively (it can't otherwise tell CapsLock is
  // already on at connect time). Synthetic sends without a real event pass
  // false — they express case through an explicit Shift code instead.
  | { type: "key"; code: string; pressed: boolean; caps: boolean }
  | { type: "viewport"; w: number; h: number }
  // The user's pick from the resolution menu a `displayModes` message offered.
  // Distinct from "viewport": a Mac's virtual display only accepts sizes off a
  // fixed list, so it is resized on request rather than followed continuously.
  | { type: "setResolution"; w: number; h: number }
  // Session control (handled by the server's session slot, not an engine):
  // pick a target from the post-login picker, or tear the session down and
  // switch back to it.
  | { type: "connect"; target: string }
  | { type: "disconnect" }
  // Clipboard bridge. The backend owns the clipboard data: "clipboard" puts
  // text on the remote's clipboard, "clipboardRequest" asks for the remote's
  // current text and is answered with a `clipboard` control message. Both are
  // sent either by the floating menu's Clipboard panel or by the automatic
  // sync in useRemoteDesktop, which pushes the local OS clipboard on focus
  // where the browser permits reading it. Nothing is retained here.
  | { type: "clipboard"; text: string }
  | { type: "clipboardRequest" };

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

// Server -> browser text frames: everything but screen tiles. `resize`/`error`
// come from the engine; `picker`/`connected` are the session-slot status the
// server sends so the browser knows which post-login state it is in.
export type ControlMsg =
  | { type: "resize"; w: number; h: number }
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
  | { type: "picker" }
  // `protocol` ("rdp"/"vnc"/"rxa") and `resize` tell the browser how to handle
  // resize: VNC follows the viewport automatically, RDP only on request.
  // `clipboard` is whether this target opted into the clipboard bridge.
  | {
      type: "connected";
      name: string;
      protocol: string;
      resize: boolean;
      clipboard: boolean;
    }
  // Whether the remote runs macOS, discovered by the engine as it connects.
  // Only the native viewer acts on it, to decide whether a local Command
  // shortcut stays Command or becomes remote Control.
  | { type: "remoteOs"; macos: boolean }
  // The remote's clipboard text: either the reply to a "clipboardRequest" or
  // an unprompted push when the remote's clipboard changed. Requested replies
  // populate the panel without silently copying; pushes retain automatic sync.
  | ({ type: "clipboard"; requested: boolean } & ClipboardSnapshot)
  // The resolutions the remote display accepts, largest first — the floating
  // menu's Resolution section, answered with "setResolution". Only the rxa
  // engine sends this, and only for a target whose Mac shares a virtual
  // display. Re-sent whenever the list changes (every reconfigure), so the
  // browser replaces its menu rather than merging; an empty list means there is
  // nothing to offer.
  | { type: "displayModes"; modes: { w: number; h: number }[] };

export interface TileMsg {
  x: number;
  y: number;
  w: number;
  h: number;
  // An encoded image stream, in `mime`.
  data: Uint8Array;
  // What `data` is, for the Blob handed to createImageBitmap. The RDP and VNC
  // engines always send PNG; the macOS agent picks per tile.
  mime: "image/png" | "image/jpeg";
}

const TILE_FRAME_KIND = 0x01;
const TILE_FORMAT_PNG = 1;
const TILE_FORMAT_JPEG = 2;
const TILE_HEADER_LEN = 10;

// Parse a binary tile frame. Layout (little-endian, matching `Tile::to_frame`
// in the backend):
//
//   offset 0: u8  frame kind, always 0x01 (tile)
//   offset 1: u8  format: 1 = PNG, 2 = JPEG
//   offset 2: u16 x | 4: u16 y | 6: u16 w | 8: u16 h
//   offset 10: payload (a PNG or JPEG stream)
//
// Returns null for anything malformed or unknown.
export function decodeTileFrame(buf: ArrayBuffer): TileMsg | null {
  if (buf.byteLength < TILE_HEADER_LEN) {
    return null;
  }
  const view = new DataView(buf);
  if (view.getUint8(0) !== TILE_FRAME_KIND) {
    return null;
  }
  let mime: TileMsg["mime"];
  switch (view.getUint8(1)) {
    case TILE_FORMAT_PNG:
      mime = "image/png";
      break;
    case TILE_FORMAT_JPEG:
      mime = "image/jpeg";
      break;
    default:
      return null;
  }
  return {
    x: view.getUint16(2, true),
    y: view.getUint16(4, true),
    w: view.getUint16(6, true),
    h: view.getUint16(8, true),
    data: new Uint8Array(buf, TILE_HEADER_LEN),
    mime,
  };
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
