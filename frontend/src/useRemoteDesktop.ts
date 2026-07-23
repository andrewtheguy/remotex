import { useCallback, useEffect, useRef, useState } from "react";
import {
  type ClientMsg,
  mouseButtonFromEvent,
  type ServerMsg,
} from "./protocol.ts";

export type ConnectionStatus =
  | "connecting"
  | "connected"
  | "closed"
  | "error";

// Placeholder hook: opens the /ws WebSocket, captures mouse + keyboard input
// over the given overlay element and sends it as ClientMsg. The backend logs
// input but sends no frames yet.
//
// TODO(phase1): decode incoming ServerMsg.tile and blit onto a canvas.
export function useRemoteDesktop(overlayRef: React.RefObject<HTMLElement | null>) {
  const [status, setStatus] = useState<ConnectionStatus>("connecting");
  const wsRef = useRef<WebSocket | null>(null);

  const send = useCallback((msg: ClientMsg) => {
    const ws = wsRef.current;
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(msg));
    }
  }, []);

  // Establish the WebSocket connection.
  useEffect(() => {
    const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
    const ws = new WebSocket(`${proto}//${window.location.host}/ws`);
    ws.binaryType = "arraybuffer";
    wsRef.current = ws;
    setStatus("connecting");

    ws.onopen = () => setStatus("connected");
    ws.onclose = () => setStatus("closed");
    ws.onerror = () => setStatus("error");
    ws.onmessage = (ev) => {
      // TODO(phase1): handle binary frames. For now, only JSON status msgs.
      if (typeof ev.data === "string") {
        try {
          const msg = JSON.parse(ev.data) as ServerMsg;
          if (msg.type === "error") console.error("server error:", msg.message);
        } catch {
          // ignore malformed
        }
      }
    };

    return () => ws.close();
  }, []);

  // Capture input over the overlay element and forward it to the server.
  useEffect(() => {
    const el = overlayRef.current;
    if (!el) return;

    // Map client coordinates to framebuffer coordinates.
    // TODO(phase1): scale by the real remote resolution once known.
    const toRemote = (e: MouseEvent) => {
      const rect = el.getBoundingClientRect();
      return {
        x: Math.round(e.clientX - rect.left),
        y: Math.round(e.clientY - rect.top),
      };
    };

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
  }, [overlayRef, send]);

  return { status };
}
