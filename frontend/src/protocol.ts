// Wire protocol shared (in shape) with the Rust backend `src/protocol.rs`.
//
// Browser -> server: input events as JSON text frames.
// Server -> browser: screen tiles as binary frames (decodeTileFrame below);
// rare control messages (resize/error) as JSON text frames with a `type` tag.

export type MouseButton = "left" | "middle" | "right";

// Browser -> server: input events captured over the remote canvas.
export type ClientMsg =
  | { type: "mouseMove"; x: number; y: number }
  | { type: "mouseButton"; button: MouseButton; pressed: boolean }
  | { type: "wheel"; dx: number; dy: number }
  | { type: "key"; code: string; pressed: boolean };

// Server -> browser text frames: everything but screen tiles.
export type ControlMsg =
  | { type: "resize"; w: number; h: number }
  | { type: "error"; message: string };

// Payload encoding of a binary tile frame.
export type TileFormat = "rgb" | "png";

export interface TileMsg {
  x: number;
  y: number;
  w: number;
  h: number;
  format: TileFormat;
  // Raw packed RGB888 (w*h*3 bytes) or a PNG stream, per `format`.
  data: Uint8Array;
}

const TILE_FRAME_KIND = 0x01;
const TILE_HEADER_LEN = 10;

// Parse a binary tile frame. Layout (little-endian, matching `Tile::to_frame`
// in the backend):
//
//   offset 0: u8  frame kind, always 0x01 (tile)
//   offset 1: u8  format (0 = raw RGB888, 1 = PNG)
//   offset 2: u16 x | 4: u16 y | 6: u16 w | 8: u16 h
//   offset 10: payload
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
  const formatByte = view.getUint8(1);
  const format: TileFormat | null =
    formatByte === 0 ? "rgb" : formatByte === 1 ? "png" : null;
  if (format === null) {
    return null;
  }
  return {
    x: view.getUint16(2, true),
    y: view.getUint16(4, true),
    w: view.getUint16(6, true),
    h: view.getUint16(8, true),
    format,
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
