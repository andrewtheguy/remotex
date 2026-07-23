// Wire protocol shared (in shape) with the Rust backend `src/protocol.rs`.
// Messages are JSON with a `type` tag.

export type MouseButton = "left" | "middle" | "right";

// Browser -> server: input events captured over the remote canvas.
export type ClientMsg =
  | { type: "mouseMove"; x: number; y: number }
  | { type: "mouseButton"; button: MouseButton; pressed: boolean }
  | { type: "wheel"; dx: number; dy: number }
  | { type: "key"; code: string; pressed: boolean };

export type TileFormat = "rgba" | "png";

// Server -> browser: screen updates and status. `data` is base64-encoded.
export type ServerMsg =
  | {
      type: "tile";
      x: number;
      y: number;
      w: number;
      h: number;
      format: TileFormat;
      data: string;
    }
  | { type: "resize"; w: number; h: number }
  | { type: "error"; message: string };

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
