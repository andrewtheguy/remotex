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
  | { type: "key"; code: string; pressed: boolean }
  | { type: "viewport"; w: number; h: number };

// Server -> browser text frames: everything but screen tiles.
export type ControlMsg =
  | { type: "resize"; w: number; h: number }
  | { type: "error"; message: string };

export interface TileMsg {
  x: number;
  y: number;
  w: number;
  h: number;
  // A PNG stream (the only tile payload encoding).
  data: Uint8Array;
}

const TILE_FRAME_KIND = 0x01;
const TILE_FORMAT_PNG = 1;
const TILE_HEADER_LEN = 10;

// Parse a binary tile frame. Layout (little-endian, matching `Tile::to_frame`
// in the backend):
//
//   offset 0: u8  frame kind, always 0x01 (tile)
//   offset 1: u8  format, always 1 (PNG) — reserved for a future codec
//   offset 2: u16 x | 4: u16 y | 6: u16 w | 8: u16 h
//   offset 10: payload (a PNG stream)
//
// Returns null for anything malformed or unknown.
export function decodeTileFrame(buf: ArrayBuffer): TileMsg | null {
  if (buf.byteLength < TILE_HEADER_LEN) {
    return null;
  }
  const view = new DataView(buf);
  if (
    view.getUint8(0) !== TILE_FRAME_KIND ||
    view.getUint8(1) !== TILE_FORMAT_PNG
  ) {
    return null;
  }
  return {
    x: view.getUint16(2, true),
    y: view.getUint16(4, true),
    w: view.getUint16(6, true),
    h: view.getUint16(8, true),
    data: new Uint8Array(buf, TILE_HEADER_LEN),
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
