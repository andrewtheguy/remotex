import { useEffect, useRef, useState } from "react";
import {
  type ClientMsg,
  mouseButtonFromEvent,
  type ServerMsg,
} from "./protocol.ts";

export type ConnectionStatus = "connecting" | "connected" | "closed" | "error";

export interface RemoteSize {
  w: number;
  h: number;
}

// Opens the /ws WebSocket, renders incoming screen tiles onto `canvasRef`, and
// forwards mouse + keyboard input captured over `overlayRef` as ClientMsg.
export function useRemoteDesktop(
  canvasRef: React.RefObject<HTMLCanvasElement | null>,
  overlayRef: React.RefObject<HTMLElement | null>,
) {
  const [status, setStatus] = useState<ConnectionStatus>("connecting");
  const [size, setSize] = useState<RemoteSize | null>(null);

  const wsRef = useRef<WebSocket | null>(null);
  const ctxRef = useRef<CanvasRenderingContext2D | null>(null);
  // Kept in a ref (not just state) so input handlers read the latest size
  // without re-subscribing.
  const sizeRef = useRef<RemoteSize | null>(null);

  const sendRef = useRef((msg: ClientMsg) => {
    const ws = wsRef.current;
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(msg));
    }
  });

  // Establish the WebSocket connection and render server messages.
  useEffect(() => {
    ctxRef.current = canvasRef.current?.getContext("2d") ?? null;

    const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
    const ws = new WebSocket(`${proto}//${window.location.host}/ws`);
    ws.binaryType = "arraybuffer";
    wsRef.current = ws;
    setStatus("connecting");

    ws.onopen = () => setStatus("connected");
    ws.onclose = () => setStatus("closed");
    ws.onerror = () => setStatus("error");
    ws.onmessage = (ev) => {
      if (typeof ev.data !== "string") return; // frames arrive as JSON text
      let msg: ServerMsg;
      try {
        msg = JSON.parse(ev.data) as ServerMsg;
      } catch {
        return;
      }
      handleServerMsg(msg);
    };

    const handleServerMsg = (msg: ServerMsg) => {
      switch (msg.type) {
        case "resize": {
          const canvas = canvasRef.current;
          if (canvas) {
            canvas.width = msg.w;
            canvas.height = msg.h;
            const ctx = canvas.getContext("2d");
            ctxRef.current = ctx;
            if (ctx) {
              ctx.fillStyle = "#000";
              ctx.fillRect(0, 0, msg.w, msg.h);
            }
          }
          const s = { w: msg.w, h: msg.h };
          sizeRef.current = s;
          setSize(s);
          break;
        }
        case "tile": {
          const ctx = ctxRef.current;
          if (!ctx || msg.format !== "rgba") return;
          const bytes = base64ToBytes(msg.data);
          const expected = msg.w * msg.h * 4;
          if (bytes.length !== expected) return; // guard against a short/garbled tile
          const image = ctx.createImageData(msg.w, msg.h);
          image.data.set(bytes);
          ctx.putImageData(image, msg.x, msg.y);
          break;
        }
        case "error": {
          console.error("rdp server error:", msg.message);
          setStatus("error");
          break;
        }
      }
    };

    return () => ws.close();
  }, [canvasRef]);

  // Capture input over the overlay element and forward it to the server,
  // scaling pointer coordinates from the displayed size to the remote size.
  useEffect(() => {
    const el = overlayRef.current;
    if (!el) return;

    const toRemote = (e: MouseEvent) => {
      const rect = el.getBoundingClientRect();
      const remote = sizeRef.current;
      const scaleX = remote && rect.width > 0 ? remote.w / rect.width : 1;
      const scaleY = remote && rect.height > 0 ? remote.h / rect.height : 1;
      return {
        x: Math.round((e.clientX - rect.left) * scaleX),
        y: Math.round((e.clientY - rect.top) * scaleY),
      };
    };

    const send = sendRef.current;
    const onMouseMove = (e: MouseEvent) => {
      const { x, y } = toRemote(e);
      send({ type: "mouseMove", x, y });
    };
    const onMouseButton = (pressed: boolean) => (e: MouseEvent) => {
      const button = mouseButtonFromEvent(e.button);
      if (button) send({ type: "mouseButton", button, pressed });
    };
    const onMouseDown = onMouseButton(true);
    const onMouseUp = onMouseButton(false);
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      send({ type: "wheel", dx: e.deltaX, dy: e.deltaY });
    };
    const onContextMenu = (e: MouseEvent) => e.preventDefault();
    const onKey = (pressed: boolean) => (e: KeyboardEvent) => {
      e.preventDefault();
      send({ type: "key", code: e.code, pressed });
    };
    const onKeyDown = onKey(true);
    const onKeyUp = onKey(false);

    el.addEventListener("mousemove", onMouseMove);
    el.addEventListener("mousedown", onMouseDown);
    el.addEventListener("mouseup", onMouseUp);
    el.addEventListener("wheel", onWheel, { passive: false });
    el.addEventListener("contextmenu", onContextMenu);
    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("keyup", onKeyUp);

    return () => {
      el.removeEventListener("mousemove", onMouseMove);
      el.removeEventListener("mousedown", onMouseDown);
      el.removeEventListener("mouseup", onMouseUp);
      el.removeEventListener("wheel", onWheel);
      el.removeEventListener("contextmenu", onContextMenu);
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("keyup", onKeyUp);
    };
  }, [overlayRef]);

  return { status, size };
}

// Decode a base64 string into a Uint8ClampedArray suitable for ImageData.
function base64ToBytes(b64: string): Uint8ClampedArray {
  const binary = atob(b64);
  const len = binary.length;
  const bytes = new Uint8ClampedArray(len);
  for (let i = 0; i < len; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}
