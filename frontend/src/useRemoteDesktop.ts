import { useCallback, useEffect, useRef, useState } from "react";
import {
  type ClientMsg,
  type ControlMsg,
  decodeTileFrame,
  type MouseButton,
  mouseButtonFromEvent,
  type TileMsg,
} from "./protocol.ts";
import {
  attachTouchGestures,
  MAX_ZOOM,
  MIN_ZOOM,
  type Point,
} from "./touchGestures.ts";

// The connection-flow state machine:
//
//   connecting ──► connected ──(drop)──► reconnecting ──► connected …
//        │              │                     │
//     (409) busy     (4001) takenOver      (409) busy
//        │              │
//     takeOver()     takeOver()            error ◄─(fatal server error)
//
// Reconnects are automatic with capped backoff; busy/takenOver/error wait for
// the user (takeOver/retry).
export type ConnectionStatus =
  | "connecting"
  | "connected"
  | "reconnecting"
  | "busy" // another browser holds the session slot (claim answered 409)
  | "takenOver" // this socket was evicted by a takeover (close code 4001)
  | "error"; // the server reported a fatal session error

export interface RemoteSize {
  w: number;
  h: number;
}

// Per-tab session identity: lets this tab reclaim its own slot after a drop
// without the takeover prompt (sessionStorage is per-tab, so two tabs of the
// same browser still contend like two browsers — as intended). Exported so
// logout (App.tsx) can drop it.
export const SESSION_KEY = "remotex.sessionId";
// Mobile sizing: pinch-zoom-capable touch devices size the session in CSS
// pixels (no dpr multiplication — a phone's 3x dpr would mint an enormous
// desktop), floored per axis at a fixed 1024x768 — so a portrait phone raises
// only the height to its own viewport, never a width-derived height that makes
// the desktop absurdly tall. The floor is a constant rather than geometry
// found on connect: the engine (and a VNC server) outlives the browser here,
// so a connect-time floor would inherit whatever damage a previous session
// left (e.g. a too-tall desktop) and never shrink it — with a constant floor
// the phone repairs it on connect.
// Exported so RemoteDesktop.tsx can switch the screen into touch layout
// (overflow hidden + viewport-fixed overlay — see index.css).
export const CAN_PINCH_ZOOM = (navigator.maxTouchPoints || 0) >= 2;
const TOUCH_MIN_WIDTH = 1024;
const TOUCH_MIN_HEIGHT = 768;

// The touch view transform: the pinch zoom and pan offset the
// gestures drive, layered on top of the fit-to-width base scale. One object
// per hook instance, mutated in place; applyCanvasCss clamps it on every
// repaint (a framebuffer resize or viewport rotation can strand a stale pan).
interface TouchViewState {
  zoom: number;
  pan: Point;
}

// Touch base scale: scale the desktop down (never up) to fit the viewport
// width.
function touchFitScale(size: RemoteSize): number {
  return Math.min(document.documentElement.clientWidth / size.w, 1);
}
// Close code sent when another browser force-claims the slot.
const CLOSE_EVICTED = 4001;
const MAX_RETRY_DELAY_MS = 15_000;

// Full-screen canvas: display the framebuffer at 1:1 device pixels —
// CSS size = remote pixels / devicePixelRatio. No scaling, no letterboxing;
// when the remote desktop is larger than the viewport the canvas overflows and
// the screen container scrolls.
function applyCanvasCss(
  canvas: HTMLCanvasElement | null,
  size: RemoteSize | null,
  view: TouchViewState,
  bottomInset = 0,
): void {
  if (!canvas || !size) {
    return;
  }
  if (CAN_PINCH_ZOOM) {
    // Touch: fit-to-width base scale with the pinch zoom on top;
    // the pan offset (≤ 0 per axis) slides the scaled desktop under the
    // viewport. Zoom and pan are clamped here — the one place every repaint
    // funnels through — and the clamped values are written back so gesture
    // math continues from what is actually on screen.
    const vw = document.documentElement.clientWidth;
    // bottomInset (the docked soft keyboard's height) shrinks the usable
    // height, so the desktop can pan up until its bottom rests just above the
    // keyboard instead of being pinned under it.
    const vh = Math.max(1, document.documentElement.clientHeight - bottomInset);
    view.zoom = Math.min(Math.max(view.zoom, MIN_ZOOM), MAX_ZOOM);
    const scale = touchFitScale(size) * view.zoom;
    const w = size.w * scale;
    const h = size.h * scale;
    view.pan = {
      x: Math.min(Math.max(view.pan.x, Math.min(0, vw - w)), 0),
      y: Math.min(Math.max(view.pan.y, Math.min(0, vh - h)), 0),
    };
    canvas.style.width = `${w}px`;
    canvas.style.height = `${h}px`;
    canvas.style.transform = `translate3d(${view.pan.x}px, ${view.pan.y}px, 0)`;
    return;
  }
  const dpr = window.devicePixelRatio || 1;
  let w = size.w / dpr;
  let h = size.h / dpr;
  // When the remote matched the viewport (dynamic resize), snap to it
  // exactly so fractional-dpr rounding can't spawn phantom scrollbars. The
  // ≤1px scale this introduces is imperceptible.
  const vw = document.documentElement.clientWidth;
  const vh = document.documentElement.clientHeight;
  if (Math.abs(w - vw) <= 1 && Math.abs(h - vh) <= 1) {
    w = vw;
    h = vh;
  }
  canvas.style.width = `${w}px`;
  canvas.style.height = `${h}px`;
}

// POST /api/session (the slot claim); null on a network failure, which the
// caller treats as retryable.
async function postClaim(force: boolean): Promise<Response | null> {
  try {
    return await fetch("/api/session", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        force,
        sessionId: sessionStorage.getItem(SESSION_KEY) ?? undefined,
      }),
    });
  } catch {
    return null;
  }
}

// The viewport report sent to the server: the desired remote
// desktop size, clamped to the protocol's u16 range. Desktop asks for the
// viewport in device pixels; touch asks for CSS pixels floored per axis at
// 1024x768 (the mobile bounds — see CAN_PINCH_ZOOM).
function viewportMsg(): Extract<ClientMsg, { type: "viewport" }> {
  const el = document.documentElement;
  const dpr = CAN_PINCH_ZOOM ? 1 : window.devicePixelRatio || 1;
  const dim = (cssPx: number, min: number) =>
    Math.min(65535, Math.max(min, Math.round(cssPx * dpr)));
  return {
    type: "viewport",
    w: dim(el.clientWidth, CAN_PINCH_ZOOM ? TOUCH_MIN_WIDTH : 1),
    h: dim(el.clientHeight, CAN_PINCH_ZOOM ? TOUCH_MIN_HEIGHT : 1),
  };
}

// Claims the single session slot (POST /api/session), opens the /ws WebSocket
// with the claim token, renders incoming screen tiles onto `canvasRef`, and
// forwards mouse + keyboard input (plus touch gestures on pinch-zoom-capable
// devices — see touchGestures.ts) captured over `overlayRef` as ClientMsg.
// Reconnects automatically after drops; busy/takenOver/error surface to the
// caller with `takeOver`/`retry` to resolve them.
//
// `onUnauthorized` fires when a claim answers 401 — the login is gone, so the
// caller swaps back to the login screen. It must be referentially stable
// (useCallback) or the connection/input effects tear down and redo. Logout is
// the floating menu's Disconnect button (see FloatingMenu.tsx), not this hook.
export function useRemoteDesktop(
  canvasRef: React.RefObject<HTMLCanvasElement | null>,
  overlayRef: React.RefObject<HTMLElement | null>,
  onUnauthorized: () => void,
) {
  const [status, setStatus] = useState<ConnectionStatus>("connecting");
  const [size, setSize] = useState<RemoteSize | null>(null);
  const [errorMessage, setErrorMessage] = useState<string | null>(null);

  const wsRef = useRef<WebSocket | null>(null);
  const ctxRef = useRef<CanvasRenderingContext2D | null>(null);
  // Kept in a ref (not just state) so input handlers read the latest size
  // without re-subscribing.
  const sizeRef = useRef<RemoteSize | null>(null);
  // The touch pinch-zoom/pan state, shared by the repaint paths (connection
  // effect) and the gesture handlers (input effect). Inert on desktop.
  const viewRef = useRef<TouchViewState>({ zoom: 1, pan: { x: 0, y: 0 } });
  // CSS pixels of canvas covered along the bottom edge by the docked soft
  // keyboard (0 when it's closed or floating). Read by every applyCanvasCss
  // call and by the gesture layer's visible-bounds math. Only meaningful on
  // touch — the non-pinch branch scrolls natively.
  const bottomInsetRef = useRef(0);
  // Lets the takeOver/retry callbacks reach into the connection driver that
  // lives inside the effect below.
  const startRef = useRef<((force: boolean) => void) | null>(null);

  const sendRef = useRef((msg: ClientMsg) => {
    const ws = wsRef.current;
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(msg));
    }
  });

  // The connection driver: claim -> WebSocket -> render, with auto-reconnect.
  useEffect(() => {
    ctxRef.current = canvasRef.current?.getContext("2d") ?? null;

    let disposed = false;
    let ws: WebSocket | null = null;
    let retryTimer: ReturnType<typeof setTimeout> | undefined;
    let attempts = 0;
    // Set when the server reports a fatal session error; stops the reconnect
    // loop so the message stays readable until the user retries.
    let fatal = false;

    const scheduleRetry = () => {
      if (disposed) {
        return;
      }
      setStatus("reconnecting");
      const delay = Math.min(1000 * 2 ** attempts, MAX_RETRY_DELAY_MS);
      attempts += 1;
      retryTimer = setTimeout(() => void connect(false), delay);
    };

    // Claim the session slot. Returns the token, "busy" when another browser
    // holds the slot (409), "unauthorized" when the login is gone (401), or
    // null for failures that should retry.
    const claim = async (
      force: boolean,
    ): Promise<string | "busy" | "unauthorized" | null> => {
      const res = await postClaim(force);
      if (!res) {
        return null;
      }
      if (res.status === 409) {
        return "busy";
      }
      if (res.status === 401) {
        return "unauthorized";
      }
      if (!res.ok) {
        return null;
      }
      try {
        const { sessionId } = (await res.json()) as { sessionId: string };
        return sessionId;
      } catch {
        return null;
      }
    };

    // Claim the session slot, then open the WebSocket with the token.
    const connect = async (force: boolean) => {
      if (disposed) {
        return;
      }
      const claimed = await claim(force);
      if (disposed) {
        return;
      }
      if (claimed === "busy") {
        setStatus("busy");
        return;
      }
      if (claimed === "unauthorized") {
        onUnauthorized(); // unmounts this hook's component
        return;
      }
      if (claimed === null) {
        scheduleRetry();
        return;
      }
      sessionStorage.setItem(SESSION_KEY, claimed);
      open(claimed);
    };

    // Viewport reports (dynamic resize), deduped per connection: a
    // resize that settles on the same size sends nothing.
    let lastViewport: RemoteSize | null = null;
    const sendViewport = () => {
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        return;
      }
      const msg = viewportMsg();
      if (
        lastViewport &&
        lastViewport.w === msg.w &&
        lastViewport.h === msg.h
      ) {
        return;
      }
      lastViewport = { w: msg.w, h: msg.h };
      ws.send(JSON.stringify(msg));
    };

    const open = (sessionId: string) => {
      const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
      const socket = new WebSocket(
        `${proto}//${window.location.host}/ws?session=${encodeURIComponent(sessionId)}`,
      );
      socket.binaryType = "arraybuffer";
      ws = socket;
      wsRef.current = socket;

      socket.onopen = () => {
        if (disposed || ws !== socket) {
          return;
        }
        setStatus("connected");
        lastViewport = null;
        sendViewport();
      };
      socket.onclose = (ev) => {
        if (disposed || ws !== socket) {
          return; // superseded by a newer connection
        }
        ws = null;
        wsRef.current = null;
        if (ev.code === CLOSE_EVICTED) {
          setStatus("takenOver");
          return;
        }
        if (fatal) {
          return; // the error state is already showing; the user retries
        }
        // Anything else — network drop, server restart, session ended, stale
        // token (4000) — goes through the reclaim + reconnect path.
        scheduleRetry();
      };
      // PNG tiles decode asynchronously (createImageBitmap), so all messages
      // are chained through one promise queue: draws land in arrival order
      // (later tiles must overwrite earlier ones) and a resize can't jump the
      // queue. The catch keeps a garbled frame from stalling the chain.
      let queue: Promise<void> = Promise.resolve();
      socket.onmessage = (ev) => {
        queue = queue.then(() => handleMessage(ev.data)).catch(() => {});
      };
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
      // A desktop arrived, so this session is healthy: reset the reconnect
      // backoff (an onopen-time reset would let a session that dies right
      // after connecting retry at full speed forever).
      attempts = 0;
      const canvas = canvasRef.current;
      const s = { w: msg.w, h: msg.h };
      if (canvas) {
        canvas.width = msg.w;
        canvas.height = msg.h;
        applyCanvasCss(canvas, s, viewRef.current, bottomInsetRef.current);
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
          console.error("remote session error:", msg.message);
          fatal = true;
          setErrorMessage(msg.message);
          setStatus("error");
          break;
      }
    };

    // User-driven (re)start: initial connect, takeover, take-back, retry.
    const start = (force: boolean) => {
      clearTimeout(retryTimer);
      fatal = false;
      attempts = 0;
      setErrorMessage(null);
      setStatus("connecting");
      if (ws) {
        const old = ws;
        ws = null; // silence its onclose before closing
        wsRef.current = null;
        old.close();
      }
      void connect(force);
    };
    startRef.current = start;
    start(false);

    // Window resizes re-report the viewport, debounced so a drag-resize sends
    // one message, not hundreds. The CSS size is re-derived too: the
    // snap-to-viewport depends on the viewport dimensions.
    let resizeTimer: ReturnType<typeof setTimeout> | undefined;
    const onViewportChange = () => {
      clearTimeout(resizeTimer);
      resizeTimer = setTimeout(() => {
        applyCanvasCss(
          canvasRef.current,
          sizeRef.current,
          viewRef.current,
          bottomInsetRef.current,
        );
        sendViewport();
      }, 250);
    };
    window.addEventListener("resize", onViewportChange);

    // devicePixelRatio changes (moving the window between monitors with
    // different scale factors, browser zoom) must re-derive the canvas CSS
    // size immediately to keep the 1:1 device-pixel mapping — they don't
    // reliably fire a resize event. matchMedia only fires when the current
    // dpr stops matching, so re-arm the query on each change.
    let dprQuery: MediaQueryList | null = null;
    const onDprChange = () => {
      watchDpr();
      applyCanvasCss(
        canvasRef.current,
        sizeRef.current,
        viewRef.current,
        bottomInsetRef.current,
      );
      onViewportChange();
    };
    const watchDpr = () => {
      dprQuery?.removeEventListener("change", onDprChange);
      dprQuery = window.matchMedia(
        `(resolution: ${window.devicePixelRatio || 1}dppx)`,
      );
      dprQuery.addEventListener("change", onDprChange);
    };
    watchDpr();

    return () => {
      disposed = true;
      startRef.current = null;
      clearTimeout(retryTimer);
      window.removeEventListener("resize", onViewportChange);
      clearTimeout(resizeTimer);
      dprQuery?.removeEventListener("change", onDprChange);
      ws?.close();
    };
  }, [canvasRef, onUnauthorized]);

  // Force-claim the slot: the takeover confirmation (busy) and the take-back
  // action after being evicted (takenOver).
  const takeOver = useCallback(() => startRef.current?.(true), []);
  // Start over without force: retry after a fatal session error.
  const retry = useCallback(() => startRef.current?.(false), []);

  // Inject a key chord from the floating toolbar — keys the browser swallows
  // (F5, Ctrl+W, Alt+F4…) or a bare modifier tap. Each DOM `code` is pressed in
  // order then released in reverse; transient, so nothing joins the held-key
  // set the input effect tracks. A no-op while the socket is down.
  const sendKeyCombo = useCallback((codes: string[]) => {
    const send = sendRef.current;
    // Synthetic sends have no CapsLock state; case is expressed by including an
    // explicit Shift code in `codes` (the soft keyboard's sticky modifier).
    for (const code of codes) {
      send({ type: "key", code, pressed: true, caps: false });
    }
    for (let i = codes.length - 1; i >= 0; i -= 1) {
      send({ type: "key", code: codes[i], pressed: false, caps: false });
    }
  }, []);

  // Report the height (CSS px) of chrome docked over the bottom of the canvas
  // — the on-screen keyboard. Re-clamps the touch view so the covered strip is
  // excluded: the desktop can pan up above it and the gesture cursor won't
  // stray under it. A no-op on desktop (no pan model); 0 clears the inset.
  const setBottomInset = useCallback(
    (px: number) => {
      bottomInsetRef.current = Math.max(0, px);
      applyCanvasCss(
        canvasRef.current,
        sizeRef.current,
        viewRef.current,
        bottomInsetRef.current,
      );
    },
    [canvasRef],
  );

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

    // Touch gestures, only on pinch-zoom-capable devices — they
    // drive the same view transform applyCanvasCss renders.
    const gestures = CAN_PINCH_ZOOM
      ? attachTouchGestures(el, {
          send,
          remoteSize: () => sizeRef.current,
          view: () => {
            const size = sizeRef.current;
            return {
              fit: size ? touchFitScale(size) : 1,
              zoom: viewRef.current.zoom,
              pan: viewRef.current.pan,
            };
          },
          applyView: (zoom, pan) => {
            viewRef.current.zoom = zoom;
            viewRef.current.pan = pan;
            applyCanvasCss(
              canvasRef.current,
              sizeRef.current,
              viewRef.current,
              bottomInsetRef.current,
            );
          },
          bottomInset: () => bottomInsetRef.current,
        })
      : null;

    const toRemote = (e: MouseEvent) => {
      // Map through the canvas rect (not the overlay): it reflects the
      // displayed framebuffer under the current touch zoom/pan, and on
      // desktop it coincides with the overlay anyway.
      const rect = (canvasRef.current ?? el).getBoundingClientRect();
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
      // Keep the gesture cursor in sync with real mouse input on hybrid
      // touch+mouse devices.
      gestures?.notePointer(x, y);
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
    // Release everything still held so nothing sticks on the remote when focus
    // leaves the surface.
    const releaseAll = () => {
      for (const code of pressedKeys) {
        // caps is irrelevant on release: the backend releases the keysym it
        // recorded at press time.
        send({ type: "key", code, pressed: false, caps: false });
      }
      pressedKeys.clear();
      for (const button of pressedButtons) {
        send({ type: "mouseButton", button, pressed: false });
      }
      pressedButtons.clear();
      gestures?.release();
    };
    const onKeyDown = (e: KeyboardEvent) => {
      e.preventDefault();
      pressedKeys.add(e.code);
      send({
        type: "key",
        code: e.code,
        pressed: true,
        caps: e.getModifierState("CapsLock"),
      });
    };
    const onKeyUp = (e: KeyboardEvent) => {
      e.preventDefault();
      pressedKeys.delete(e.code);
      send({
        type: "key",
        code: e.code,
        pressed: false,
        caps: e.getModifierState("CapsLock"),
      });
    };
    const onBlur = () => releaseAll();

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
      gestures?.detach();
      el.removeEventListener("mousemove", onMouseMove);
      el.removeEventListener("mousedown", onMouseDown);
      window.removeEventListener("mouseup", onMouseUp);
      el.removeEventListener("wheel", onWheel);
      el.removeEventListener("contextmenu", onContextMenu);
      el.removeEventListener("keydown", onKeyDown);
      el.removeEventListener("keyup", onKeyUp);
      el.removeEventListener("blur", onBlur);
    };
  }, [overlayRef, canvasRef]);

  return {
    status,
    size,
    errorMessage,
    takeOver,
    retry,
    sendKeyCombo,
    setBottomInset,
  };
}
