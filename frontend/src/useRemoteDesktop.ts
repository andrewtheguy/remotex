import { useEffect, useRef, useState } from "react";
import {
  type ClientMsg,
  type ControlMsg,
  decodeTileFrame,
  type MouseButton,
  mouseButtonFromEvent,
  type TileMsg,
} from "./protocol.ts";

export type ConnectionStatus = "connecting" | "connected" | "closed" | "error";

export interface RemoteSize {
  w: number;
  h: number;
}

// Phase 3 (full-screen canvas): display the framebuffer at 1:1 device pixels —
// CSS size = remote pixels / devicePixelRatio. No scaling, no letterboxing;
// when the remote desktop is larger than the viewport the canvas overflows and
// the screen container scrolls.
function applyCanvasCss(
  canvas: HTMLCanvasElement | null,
  size: RemoteSize | null,
): void {
  if (!canvas || !size) {
    return;
  }
  const dpr = window.devicePixelRatio || 1;
  canvas.style.width = `${size.w / dpr}px`;
  canvas.style.height = `${size.h / dpr}px`;
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
    // PNG tiles decode asynchronously (createImageBitmap), so all messages are
    // chained through one promise queue: draws land in arrival order (later
    // tiles must overwrite earlier ones) and a resize can't jump the queue.
    let queue: Promise<void> = Promise.resolve();
    ws.onmessage = (ev) => {
      // The catch keeps a garbled frame from stalling the chain.
      queue = queue.then(() => handleMessage(ev.data)).catch(() => {});
    };

    const handleMessage = async (data: unknown) => {
      if (typeof data === "string") {
        let msg: ControlMsg;
        try {
          msg = JSON.parse(data) as ControlMsg;
        } catch {
          return;
        }
        handleControlMsg(msg);
        return;
      }
      if (data instanceof ArrayBuffer) {
        const tile = decodeTileFrame(data);
        if (tile) {
          await drawTile(tile);
        }
      }
    };

    const drawTile = async (tile: TileMsg) => {
      const ctx = ctxRef.current;
      if (!ctx) {
        return;
      }
      const bitmap = await createImageBitmap(
        new Blob([tile.data as Uint8Array<ArrayBuffer>], {
          type: "image/png",
        }),
      );
      ctx.drawImage(bitmap, tile.x, tile.y);
      bitmap.close();
    };

    const handleResize = (msg: Extract<ControlMsg, { type: "resize" }>) => {
      const canvas = canvasRef.current;
      const s = { w: msg.w, h: msg.h };
      if (canvas) {
        canvas.width = msg.w;
        canvas.height = msg.h;
        applyCanvasCss(canvas, s);
        const ctx = canvas.getContext("2d");
        ctxRef.current = ctx;
        if (ctx) {
          ctx.fillStyle = "#000";
          ctx.fillRect(0, 0, msg.w, msg.h);
        }
      }
      sizeRef.current = s;
      setSize(s);
    };

    const handleControlMsg = (msg: ControlMsg) => {
      switch (msg.type) {
        case "resize":
          handleResize(msg);
          break;
        case "error":
          console.error("rdp server error:", msg.message);
          setStatus("error");
          break;
      }
    };

    return () => ws.close();
  }, [canvasRef]);

  // devicePixelRatio changes (moving the window between monitors with
  // different scale factors, browser zoom) must re-derive the canvas CSS size
  // to keep the 1:1 device-pixel mapping. matchMedia only fires when the
  // current dpr stops matching, so re-arm the query on each change.
  useEffect(() => {
    let query: MediaQueryList | null = null;
    const onDprChange = () => {
      watch();
      applyCanvasCss(canvasRef.current, sizeRef.current);
    };
    const watch = () => {
      query?.removeEventListener("change", onDprChange);
      query = window.matchMedia(
        `(resolution: ${window.devicePixelRatio || 1}dppx)`,
      );
      query.addEventListener("change", onDprChange);
    };
    watch();
    return () => query?.removeEventListener("change", onDprChange);
  }, [canvasRef]);

  // Capture input over the overlay element and forward it to the server,
  // scaling pointer coordinates from the displayed size to the remote size.
  useEffect(() => {
    const el = overlayRef.current;
    if (!el) {
      return;
    }

    const send = sendRef.current;
    // Track what's held so we can release it if focus/pointer leaves the surface,
    // avoiding keys/buttons that stick down on the remote host.
    const pressedButtons = new Set<MouseButton>();
    const pressedKeys = new Set<string>();

    const toRemote = (e: MouseEvent) => {
      const rect = el.getBoundingClientRect();
      const remote = sizeRef.current;
      const scaleX = remote && rect.width > 0 ? remote.w / rect.width : 1;
      const scaleY = remote && rect.height > 0 ? remote.h / rect.height : 1;
      let x = Math.round((e.clientX - rect.left) * scaleX);
      let y = Math.round((e.clientY - rect.top) * scaleY);
      // Clamp to the framebuffer bounds so a drag past the edge stays in range.
      if (remote) {
        x = Math.min(Math.max(x, 0), remote.w - 1);
        y = Math.min(Math.max(y, 0), remote.h - 1);
      }
      return { x, y };
    };

    const onMouseMove = (e: MouseEvent) => {
      const { x, y } = toRemote(e);
      send({ type: "mouseMove", x, y });
    };
    const onMouseDown = (e: MouseEvent) => {
      el.focus(); // take keyboard focus on pointer interaction
      const button = mouseButtonFromEvent(e.button);
      if (!button) {
        return;
      }
      pressedButtons.add(button);
      send({ type: "mouseButton", button, pressed: true });
    };
    // Release on window so a press that ends outside the overlay still reports
    // the button up. Only buttons we saw pressed on the surface are released.
    const onMouseUp = (e: MouseEvent) => {
      const button = mouseButtonFromEvent(e.button);
      if (!button || !pressedButtons.delete(button)) {
        return;
      }
      send({ type: "mouseButton", button, pressed: false });
    };
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      send({ type: "wheel", dx: e.deltaX, dy: e.deltaY });
    };
    const onContextMenu = (e: MouseEvent) => e.preventDefault();
    const onKeyDown = (e: KeyboardEvent) => {
      e.preventDefault();
      pressedKeys.add(e.code);
      send({ type: "key", code: e.code, pressed: true });
    };
    const onKeyUp = (e: KeyboardEvent) => {
      e.preventDefault();
      pressedKeys.delete(e.code);
      send({ type: "key", code: e.code, pressed: false });
    };
    // On blur, release everything still held so nothing sticks on the remote.
    const onBlur = () => {
      for (const code of pressedKeys) {
        send({ type: "key", code, pressed: false });
      }
      pressedKeys.clear();
      for (const button of pressedButtons) {
        send({ type: "mouseButton", button, pressed: false });
      }
      pressedButtons.clear();
    };

    el.addEventListener("mousemove", onMouseMove);
    el.addEventListener("mousedown", onMouseDown);
    window.addEventListener("mouseup", onMouseUp);
    el.addEventListener("wheel", onWheel, { passive: false });
    el.addEventListener("contextmenu", onContextMenu);
    // Keyboard is scoped to the focused overlay (not window) so the remote
    // surface only grabs keys when the user is interacting with it.
    el.addEventListener("keydown", onKeyDown);
    el.addEventListener("keyup", onKeyUp);
    el.addEventListener("blur", onBlur);

    return () => {
      el.removeEventListener("mousemove", onMouseMove);
      el.removeEventListener("mousedown", onMouseDown);
      window.removeEventListener("mouseup", onMouseUp);
      el.removeEventListener("wheel", onWheel);
      el.removeEventListener("contextmenu", onContextMenu);
      el.removeEventListener("keydown", onKeyDown);
      el.removeEventListener("keyup", onKeyUp);
      el.removeEventListener("blur", onBlur);
    };
  }, [overlayRef]);

  return { status, size };
}
