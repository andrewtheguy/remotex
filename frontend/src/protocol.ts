// Wire protocol shared (in shape) with the Rust backend `src/protocol.rs`.
//
// Browser -> server: input events as JSON text frames.
// Server -> browser: screen tiles as binary frames (decodeTileFrame below);
// rare control messages (resize/error) as JSON text frames with a `type` tag.

export type MouseButton = "left" | "middle" | "right";

// Browser -> server: input events captured over the remote canvas, plus
// viewport reports (the desired remote desktop size, in the *remote's* pixels:
// the room the browser has times the density the remote draws at — engines that
// support dynamic resize act on them, the rest ignore them).
export type ClientMsg =
  | { type: "mouseMove"; x: number; y: number }
  | { type: "mouseButton"; button: MouseButton; pressed: boolean }
  | { type: "wheel"; dx: number; dy: number }
  // `caps` carries KeyboardEvent.getModifierState("CapsLock") so the backend
  // knows the lock state authoritatively (it can't otherwise tell CapsLock is
  // already on at connect time). Synthetic sends without a real event pass
  // false — they express case through an explicit Shift code instead.
  | { type: "key"; code: string; pressed: boolean; caps: boolean }
  // The only way to ask for a remote size, and deliberately not paired with a
  // menu of resolutions: a remote's resolution belongs to the machine running
  // it. The two engines that act on this are the two whose protocols hand that
  // decision to the client — VNC continuously, RDP on request.
  | { type: "viewport"; w: number; h: number }
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
  | { type: "clipboardRequest" }
  // Share a different one of the remote's displays, by the `id` of an entry
  // from the last `displays` control message.
  //
  // The counterpart to "viewport" above, and the contrast is the point: a
  // remote's *resolution* belongs to the machine running it, while *which of
  // its screens to look at* is only a question for the person looking. Only rxa
  // answers it — RDP and VNC each deliver one framebuffer spanning every remote
  // screen — so for other protocols no display list arrives and no picker is
  // shown.
  | { type: "selectDisplay"; id: number };

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
