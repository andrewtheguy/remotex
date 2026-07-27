import { useCallback, useEffect, useRef, useState } from "react";
import {
  type ClientMsg,
  type ClipboardSnapshot,
  type ControlMsg,
  type DisplayInfo,
  decodeTileFrame,
  MAX_CLIPBOARD_BYTES,
  type MouseButton,
  mouseButtonFromEvent,
  type RemoteClipboard,
  type TileMsg,
} from "./protocol.ts";
import {
  attachTouchGestures,
  MAX_ZOOM,
  MIN_ZOOM,
  type Point,
} from "./touchGestures.ts";

// The WebSocket/claim connection-flow state machine (independent of the
// picker-vs-desktop `mode` the attached socket carries):
//
//   connecting ──► connected ──(drop)──► reconnecting ──► connected …
//        │              │                     │
//     (409) busy     (4001) takenOver      (409) busy
//        │              │
//     takeOver()     takeOver()
//
// Reconnects are automatic with capped backoff; busy/takenOver wait for the
// user (takeOver). A fatal engine error is no longer a connection state — the
// socket stays up and the session returns to the picker with the error shown
// there (see `connectError`).
export type ConnectionStatus =
  | "connecting"
  | "connected"
  | "reconnecting"
  | "busy" // another browser holds the session slot (claim answered 409)
  | "takenOver"; // this socket was evicted by a takeover (close code 4001)

// Which post-login state the attached session is in, driven by the server's
// `picker`/`connected` status messages: the target picker, or a live desktop.
export type SessionMode = "picker" | "desktop";

export interface RemoteSize {
  w: number;
  h: number;
  // How many of those pixels the remote draws per point of its own desktop: 1 for
  // VNC, RDP and a 1x Mac, 2 for a Retina one. Here rather than alongside because
  // neither number presents the desktop without the other.
  scale: number;
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
const clipboardEncoder = new TextEncoder();

// Whether `text` exceeds one clipboard transfer's ceiling.
//
// The length test comes first and is not just a fast path: UTF-8 never spends
// fewer bytes than the string has UTF-16 code units, so anything longer than the
// ceiling is already over it — and that decides the huge cases without encoding
// a copy of the string to measure it.
function overClipboardLimit(text: string): boolean {
  return (
    text.length > MAX_CLIPBOARD_BYTES ||
    clipboardEncoder.encode(text).byteLength > MAX_CLIPBOARD_BYTES
  );
}

// Close code sent when another browser force-claims the slot.
const CLOSE_EVICTED = 4001;
const MAX_RETRY_DELAY_MS = 15_000;

// How long `requestClipboard` waits for the server's answer before giving up.
// Generous because it is not all local: a VNC or RDP target answers from an
// engine-side buffer immediately, but an `rxa` target is a real round trip to
// the Mac, and one made during an agent reconnect is discarded outright and
// never answered at all.
const CLIPBOARD_FETCH_TIMEOUT_MS = 5000;

// Full-screen canvas: display the framebuffer at the remote's own size —
// CSS size = remote pixels / the remote's density. The browser rasterizes that
// for whichever screen it is on, so the picture is scaled by the ratio between
// the two densities, automatically and in both directions:
//
//   1x guest,     Retina host -> magnified 2x, soft (as every remote desktop
//                                client is: there are no more pixels to have)
//   Retina guest, 1x host     -> reduced to half, downsampled and sharp
//   equal densities           -> one framebuffer pixel per device pixel,
//                                nothing resampled
//
// Dragging the window to a display of a different density switches between
// those on its own, which is why `devicePixelRatio` appears nowhere in this
// file. Following the host's density is the behaviour; not naming it is how it
// is obtained. Dividing by it here, as this did once, is what *broke* the table
// above — the canvas was then sized in the host's device pixels, so a 1x guest
// came out at half its physical size on a Retina screen instead of magnified.
//
// No letterboxing; when the desktop is larger than the viewport the canvas
// overflows and the screen container scrolls.
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
  const density = size.scale > 0 ? size.scale : 1;
  let w = size.w / density;
  let h = size.h / density;
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

// ── Pointer rendering ───────────────────────────────────────────────────────
// Engines whose server hands the cursor shape over instead of drawing it into
// the framebuffer (VNC's Cursor pseudo-encoding — macOS Screen Sharing never
// draws one) make the browser responsible for the pointer: the hardware
// pointer wears the shape as a CSS cursor, and the touch gesture layer's
// virtual pointer gets an image drawn at its remote position. Engines that
// composite the pointer themselves (RDP, and VNC servers that ignore the
// pseudo-encoding) send no `cursor` message at all, and the browser keeps its
// own pointer hidden — see index.css.

interface CursorImage {
  url: string;
  /** Hotspot within the image, in cursor pixels. */
  hx: number;
  hy: number;
  w: number;
  h: number;
}

// How small the virtual pointer may get on screen, in CSS pixels across its
// longer side. Zoomed out to fit a phone, a pointer drawn at the desktop's own
// scale is a few pixels across, and the pointer is the one thing on screen that
// has to stay findable. Deliberately low: the pointer should read as part of
// the desktop it sits on, so the floor is a last resort rather than a size.
const MIN_POINTER_CSS_PX = 14;

// The engine's pointer state. `image` is null when the remote hid the pointer;
// the state as a whole is null while the remote is drawing it itself.
interface RemoteCursor {
  image: CursorImage | null;
}

let arrow: CursorImage | null = null;

// A neutral arrow, standing in when the browser owns the pointer but the
// remote has hidden its shape — on a remote desktop a pointer you can't see is
// worse than a generic one. Painted into a canvas rather than carried as an
// embedded blob, and PNG rather than SVG because Safari rejects SVG cursors.
function fallbackCursor(): CursorImage {
  if (arrow) {
    return arrow;
  }
  const w = 12;
  const h = 19;
  const canvas = document.createElement("canvas");
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext("2d");
  if (ctx) {
    // The usual arrow, on half-pixel coordinates so the 1px outline lands on
    // whole pixels. The tip is the hotspot, hence (0, 0).
    ctx.beginPath();
    ctx.moveTo(0.5, 0.5);
    ctx.lineTo(0.5, 16);
    ctx.lineTo(4, 12.5);
    ctx.lineTo(6.5, 18);
    ctx.lineTo(9, 17);
    ctx.lineTo(6.5, 11.5);
    ctx.lineTo(11, 11.5);
    ctx.closePath();
    // White with a black outline, so it reads against any remote background.
    ctx.fillStyle = "#fff";
    ctx.fill();
    ctx.strokeStyle = "#000";
    ctx.lineWidth = 1;
    ctx.stroke();
  }
  arrow = { url: canvas.toDataURL("image/png"), hx: 0, hy: 0, w, h };
  return arrow;
}

// A cursor image as a CSS url() token. An unquoted token ends at the first
// `)`, so quoting (and escaping what would close the quote) keeps the image
// string from spilling into the declaration. Our own base64 can't contain
// either character, but the URL is server-supplied and this is one line.
function cssUrl(url: string): string {
  return `url("${url.replace(/["\\]/g, "\\$&")}")`;
}

// What to draw for the pointer, or null to leave it to the remote.
function cursorImage(remote: RemoteCursor | null): CursorImage | null {
  if (!remote) {
    return null;
  }
  return remote.image ?? fallbackCursor();
}

// Wear the shape as an element's CSS cursor, sized to `view` — the desktop's
// on-screen scale, framebuffer pixels to CSS pixels.
//
// A cursor image is laid out at its intrinsic size in CSS pixels, and shapes
// arrive in remote framebuffer pixels, so on a Retina remote the hardware
// pointer came out at twice the size of the desktop it sits on (a half-scale
// canvas, a full-scale pointer). `cursor` has no width to set, so the scale
// travels as an image-set() resolution instead: 2x says "two image pixels per
// CSS pixel", which is what a 2x shape on a half-scale canvas is. The hotspot
// scales with it — those coordinates are in CSS pixels of the laid-out image,
// not in image pixels.
//
// Written twice on purpose. image-set() inside `cursor` is not universal, and an
// unsupported value is rejected by the CSSOM rather than applied, which leaves
// the plain url() standing: a pointer at the wrong size beats no pointer at all,
// which is what this element's stylesheet `cursor: none` would otherwise give.
function applyCursorCss(el: HTMLElement, image: CursorImage, view: number) {
  el.style.cursor = `${cssUrl(image.url)} ${image.hx} ${image.hy}, default`;
  if (!(view > 0) || !Number.isFinite(view) || Math.abs(view - 1) < 0.01) {
    return;
  }
  const density = (1 / view).toFixed(3);
  const hx = Math.round(image.hx * view);
  const hy = Math.round(image.hy * view);
  el.style.cursor = `image-set(${cssUrl(image.url)} ${density}x) ${hx} ${hy}, default`;
}

// Push the pointer state to the DOM: the CSS cursor on the input overlay (it
// tracks the hardware pointer with no lag) and the image element the touch
// gesture layer's virtual pointer rides on. `touchAt` is that virtual position
// in remote pixels, null while a hardware mouse is driving.
function paintCursor(
  els: {
    overlay: HTMLElement | null;
    canvas: HTMLCanvasElement | null;
    pointer: HTMLImageElement | null;
  },
  size: RemoteSize | null,
  remote: RemoteCursor | null,
  touchAt: Point | null,
): void {
  const image = cursorImage(remote);
  const rect = els.canvas?.getBoundingClientRect();
  // The desktop's on-screen scale, framebuffer pixels to CSS pixels: both
  // pointers are sized through it. 1:1 until the first resize names a size.
  const view = rect && size && size.w > 0 ? rect.width / size.w : 1;
  if (els.overlay) {
    if (image) {
      applyCursorCss(els.overlay, image, view);
    } else {
      els.overlay.style.cursor = "none";
    }
  }
  const pointer = els.pointer;
  if (!pointer) {
    return;
  }
  if (!image || !touchAt || !rect || !size || size.w <= 0) {
    pointer.style.display = "none";
    return;
  }
  // `view` places the virtual pointer on its remote position and sizes it to
  // match the desktop under it, floored at a minimum size so it stays visible
  // when the desktop is scaled down to fit.
  //
  // The floor is an on-screen size, not one CSS pixel per cursor pixel: pinning
  // a 2x shape to 1:1 drew a pointer several times the size of everything
  // around it on a phone.
  const extent = Math.max(image.w, image.h);
  const draw = extent > 0 ? Math.max(view, MIN_POINTER_CSS_PX / extent) : view;
  if (pointer.src !== image.url) {
    pointer.src = image.url;
  }
  pointer.style.width = `${image.w * draw}px`;
  pointer.style.height = `${image.h * draw}px`;
  pointer.style.transform = `translate3d(${
    rect.left + touchAt.x * view - image.hx * draw
  }px, ${rect.top + touchAt.y * view - image.hy * draw}px, 0)`;
  pointer.style.display = "block";
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
// desktop size, clamped to the protocol's u16 range. Desktop asks in the
// remote's own pixels — its room in CSS pixels times the density the remote
// draws at, so a desktop that comes back fills the window and is shown one point
// per point. Touch asks for CSS pixels floored per axis at 1024x768 (the mobile
// bounds — see CAN_PINCH_ZOOM), where fit-to-width decides the presentation
// instead.
function viewportMsg(
  guestScale: number,
): Extract<ClientMsg, { type: "viewport" }> {
  const el = document.documentElement;
  const density = CAN_PINCH_ZOOM || !(guestScale > 0) ? 1 : guestScale;
  const dim = (cssPx: number, min: number) =>
    Math.min(65535, Math.max(min, Math.round(cssPx * density)));
  return {
    type: "viewport",
    w: dim(el.clientWidth, CAN_PINCH_ZOOM ? TOUCH_MIN_WIDTH : 1),
    h: dim(el.clientHeight, CAN_PINCH_ZOOM ? TOUCH_MIN_HEIGHT : 1),
  };
}

// Claims the single session slot (POST /api/session) and opens the /ws
// WebSocket with the claim token. The attached socket starts in the post-login
// **picker** (`mode === "picker"`): call `connect(name)` to start a target
// session (`mode` flips to "desktop"), and `switchTarget()` to tear it down and
// return to the picker. In desktop mode it renders incoming screen tiles onto
// `canvasRef` and forwards mouse + keyboard input (plus touch gestures on
// pinch-zoom-capable devices — see touchGestures.ts) captured over `overlayRef`
// as ClientMsg. Reconnects automatically after drops; busy/takenOver surface to
// the caller with `takeOver` to resolve them.
//
// `pointerRef` is the image element the client-side pointer is drawn on when
// the engine sends cursor shapes rather than compositing them (see
// paintCursor); it is positioned imperatively and stays hidden otherwise.
//
// `onUnauthorized` fires when a claim answers 401 — the login is gone, so the
// caller swaps back to the login screen. It must be referentially stable
// (useCallback) or the connection/input effects tear down and redo. Logout is
// the floating menu's Log out button (see FloatingMenu.tsx), not this hook.
export function useRemoteDesktop(
  canvasRef: React.RefObject<HTMLCanvasElement | null>,
  overlayRef: React.RefObject<HTMLElement | null>,
  pointerRef: React.RefObject<HTMLImageElement | null>,
  onUnauthorized: () => void,
) {
  const [status, setStatus] = useState<ConnectionStatus>("connecting");
  const [size, setSize] = useState<RemoteSize | null>(null);
  // Picker vs desktop. `connectError` holds the last engine error to show
  // against the picker after a failed connect.
  const [mode, setMode] = useState<SessionMode>("picker");
  const [connectError, setConnectError] = useState<string | null>(null);
  // The target a connect() is waiting on, so the picker can show progress
  // until the server answers with `connected` (or an error).
  const [pendingTarget, setPendingTarget] = useState<string | null>(null);
  // True when the connected target supports resize but only on request (RDP):
  // the floating menu shows a "Resize to window" button and automatic viewport
  // reports are suppressed. VNC resizes automatically, so it stays false.
  const [canResize, setCanResize] = useState(false);
  // True when the connected target opted into the clipboard bridge, which is
  // what enables the floating menu's Clipboard button.
  const [canClipboard, setCanClipboard] = useState(false);
  // The remote's displays and which one it is sharing, as the remote last
  // reported them. Empty for every engine that cannot offer a choice, which is
  // what hides the picker rather than a separate capability flag: a list of one
  // is nothing to choose between either.
  //
  // Never written optimistically. A click sends a "selectDisplay" and the
  // checkmark moves only when the remote says it moved, so a refused selection
  // leaves the panel agreeing with what is on screen.
  const [displays, setDisplays] = useState<DisplayInfo[]>([]);
  const [activeDisplayId, setActiveDisplayId] = useState<number | null>(null);
  // The last remote clipboard snapshot, whether fetched or pushed, and a
  // counter that ticks on every arrival. Fetching the same text twice must
  // still register as an answer, and a null-vs-string flag cannot express that.
  const [remoteClipboard, setRemoteClipboard] =
    useState<RemoteClipboard | null>(null);
  // The two halves of the automatic sync's echo guard: text last received from
  // the remote (so it is never sent straight back), and text last sent to the
  // remote (so a server that echoes a cut back at us does not bounce forever).
  // Refs, not state — they gate effects and must not re-run them.
  const lastFromRemoteRef = useRef<string | null>(null);
  const lastToRemoteRef = useRef<string | null>(null);
  // Callers of `requestClipboard` waiting on the server's requested reply.
  // Unsolicited pushes update the snapshot and automatic sync but deliberately
  // leave these pending, so a push racing a panel open cannot masquerade as the
  // read that button requested.
  const clipboardWaitersRef = useRef<
    ((snapshot: ClipboardSnapshot | null) => void)[]
  >([]);

  // Settle everyone waiting on a fetch. `null` means "no answer came".
  const settleClipboardWaiters = useCallback(
    (snapshot: ClipboardSnapshot | null) => {
      const waiters = clipboardWaitersRef.current;
      clipboardWaitersRef.current = [];
      for (const settle of waiters) {
        settle(snapshot);
      }
    },
    [],
  );

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
  // Manual-resize mode (RDP with resize enabled): while set, automatic viewport
  // reports are suppressed and only the menu's "Resize to window" sends one.
  const manualResizeRef = useRef(false);
  // Set by the connection effect so the menu's "Resize to window" can push the
  // current viewport even in manual-resize mode.
  const resizeToWindowRef = useRef<(() => void) | null>(null);

  // The engine's latest pointer state, and where the touch gesture layer's
  // virtual pointer sits (null while a hardware mouse is driving). Both are
  // refs: they are pushed straight to the DOM by syncCursor, and no React
  // output depends on them — pointer motion must not re-render.
  const cursorRef = useRef<RemoteCursor | null>(null);
  const touchCursorRef = useRef<Point | null>(null);

  // Re-apply the pointer to the DOM. Called whenever the shape, the virtual
  // pointer position, or the canvas geometry (resize, zoom, pan, dpr) changes.
  const syncCursor = useCallback(() => {
    paintCursor(
      {
        overlay: overlayRef.current,
        canvas: canvasRef.current,
        pointer: pointerRef.current,
      },
      sizeRef.current,
      cursorRef.current,
      touchCursorRef.current,
    );
  }, [canvasRef, overlayRef, pointerRef]);

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

    // Drop any retained framebuffer. Every interstitial (connecting,
    // reconnecting, busy, taken over) and the picker fully hide the canvas
    // behind a solid overlay, and the desktop is always rebuilt from a full
    // repaint (the server's Refresh) on the next connect — so there is nothing
    // worth keeping behind the overlay, and holding stale pixels would only
    // flash on the way back.
    const clearDesktop = () => {
      sizeRef.current = null;
      setSize(null);
      const canvas = canvasRef.current;
      if (canvas) {
        canvas.width = 0;
        canvas.height = 0;
      }
      ctxRef.current = null;
      // Pointer ownership is per-engine: the next target may well composite
      // its own cursor, so drop back to hiding the browser's until it says
      // otherwise. A reattach to the same engine gets the shape replayed.
      cursorRef.current = null;
      touchCursorRef.current = null;
      syncCursor();
    };

    const scheduleRetry = () => {
      if (disposed) {
        return;
      }
      clearDesktop();
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
        clearDesktop();
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
    // resize that settles on the same size sends nothing. In manual-resize mode
    // (RDP) the automatic callers are suppressed — an RDP resize triggers a
    // heavy Deactivation-Reactivation, so it happens only when the user asks
    // (`manual: true`, from the menu's "Resize to window").
    let lastViewport: { w: number; h: number } | null = null;
    const sendViewport = (opts?: { manual?: boolean }) => {
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        return;
      }
      if (!opts?.manual && manualResizeRef.current) {
        return;
      }
      const msg = viewportMsg(sizeRef.current?.scale ?? 1);
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
    // The manual "Resize to window" action: report the viewport even in
    // manual-resize mode. Dedup still applies, so re-clicking at the same
    // window size won't fire a redundant resize.
    resizeToWindowRef.current = () => sendViewport({ manual: true });

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
        // The viewport is (re)sent from the `connected` handler, once the
        // protocol is known — a running RDP engine must not be resized by an
        // automatic report fired blindly on reconnect.
        lastViewport = null;
      };
      socket.onclose = (ev) => {
        if (disposed || ws !== socket) {
          return; // superseded by a newer connection
        }
        ws = null;
        wsRef.current = null;
        // Before either branch below: the link that owed us a clipboard reply
        // is gone, so fail any fetch now rather than leaving the button on
        // "Fetching…" until its timeout expires for an answer that cannot come.
        settleClipboardWaiters(null);
        if (ev.code === CLOSE_EVICTED) {
          clearDesktop();
          setStatus("takenOver");
          return;
        }
        // Anything else — network drop, server restart, stale token (4000) —
        // goes through the reclaim + reconnect path. (An engine that ends no
        // longer closes the socket; the server returns it to the picker.)
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
          type: tile.mime,
        }),
      );
      ctx.drawImage(bitmap, tile.x, tile.y);
      bitmap.close();
    };

    const handleResize = (msg: Extract<ControlMsg, { type: "resize" }>) => {
      const canvas = canvasRef.current;
      const s = { w: msg.w, h: msg.h, scale: msg.scale > 0 ? msg.scale : 1 };
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
      syncCursor();
    };

    const mirrorRemoteClipboard = (text: string) => {
      if (text === "") {
        return;
      }
      const alreadyMirrored = text === lastFromRemoteRef.current;
      const echoedFromHost = text === lastToRemoteRef.current;
      lastFromRemoteRef.current = text;
      if (alreadyMirrored || echoedFromHost) {
        return;
      }
      void navigator.clipboard?.writeText?.(text).catch(() => {});
    };

    const handleControlMsg = (msg: ControlMsg) => {
      // Any control message proves the socket attached to the slot, so reset
      // the reconnect backoff (an onopen-time reset would let a slot that
      // closes right after connecting retry at full speed forever).
      attempts = 0;
      switch (msg.type) {
        case "resize":
          handleResize(msg);
          break;
        case "cursor":
          // The engine hands over the pointer shape, which means its server is
          // not drawing one — from here the browser owns pointer rendering. A
          // null image is the remote hiding it; fallbackCursor stands in.
          cursorRef.current = {
            image: msg.image
              ? {
                  url: `data:image/png;base64,${msg.image}`,
                  hx: msg.hx,
                  hy: msg.hy,
                  w: msg.w,
                  h: msg.h,
                }
              : null,
          };
          syncCursor();
          break;
        case "error":
          // The engine failed (or a connect was refused); show the message
          // against the picker rather than a dead-end error screen, and end any
          // pending connect so the picker re-enables. An engine failure is
          // followed by `picker`; a refusal leaves the slot already there.
          console.error("remote session error:", msg.message);
          setConnectError(msg.message);
          setPendingTarget(null);
          break;
        case "connected": {
          // A target session started (picker connect, reattach, or takeover of
          // a live desktop): switch to the desktop.
          setConnectError(null);
          setPendingTarget(null);
          setMode("desktop");
          // Three behaviours, decided here and nowhere else. VNC follows the
          // viewport automatically. RDP resizes only when asked, because its
          // reactivation is heavy — so viewport reports are suppressed and the
          // menu's "Resize to window" is the one caller. Everything else,
          // including every Mac, keeps whatever size the remote is at: its
          // resolution is set on the remote.
          const manual = msg.protocol === "rdp" && msg.resize;
          manualResizeRef.current = manual || msg.protocol === "rxa";
          setCanResize(manual);
          setCanClipboard(msg.clipboard);
          // A freshly-started engine needs the current viewport; the report is
          // sent here (once the protocol is known), undeduped.
          lastViewport = null;
          sendViewport();
          break;
        }
        case "clipboard": {
          // Both paths update the panel, but only unsolicited pushes mirror
          // into the browser's OS clipboard. Opening/revealing the panel is a
          // read action; its explicit Copy button is the consent boundary for
          // changing the local clipboard.
          const { text, changedAtMs, requested, oversizedBytes } = msg;
          const snapshot = { text, changedAtMs, oversizedBytes };
          setRemoteClipboard((prev) => ({
            ...snapshot,
            seq: (prev?.seq ?? 0) + 1,
          }));
          if (requested) {
            // Release anyone blocked on this Fetch; a racing unsolicited push
            // remains an automatic-sync event and does not open the panel
            // before the actual response arrives.
            settleClipboardWaiters(snapshot);
          } else {
            // A copy on the remote is immediately pastable here. Best effort
            // by design: `writeText` is absent on a non-secure origin and can
            // reject when the tab is unfocused. Never mirror an empty push,
            // which would wipe the local clipboard for a non-text remote copy —
            // a refused oversized copy arrives as one of those, and the panel
            // is where its size is reported.
            mirrorRemoteClipboard(text);
          }
          break;
        }
        case "displays":
          setDisplays(msg.displays);
          setActiveDisplayId(msg.active);
          break;
        case "remoteOs":
          // Ignored here. Whether the remote is a Mac only decides a keyboard
          // convention (does a local Command shortcut stay Command or become
          // remote Control), and the browser never translates shortcuts — it
          // forwards the physical `code` it saw. The macOS viewer is the client
          // that acts on this.
          break;
        case "picker":
          // No target selected (idle attach, switch-target, or an engine that
          // ended): show the picker. Drop any retained framebuffer so a later
          // connect starts from a clean "waiting for the desktop" state.
          setPendingTarget(null);
          setMode("picker");
          manualResizeRef.current = false;
          setCanResize(false);
          setCanClipboard(false);
          setDisplays([]);
          setActiveDisplayId(null);
          setRemoteClipboard(null);
          lastFromRemoteRef.current = null;
          lastToRemoteRef.current = null;
          // No engine left to answer a fetch that is still in flight.
          settleClipboardWaiters(null);
          clearDesktop();
          break;
      }
    };

    // User-driven (re)start: initial connect, takeover, take-back.
    const start = (force: boolean) => {
      clearTimeout(retryTimer);
      attempts = 0;
      clearDesktop();
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
        syncCursor();
        sendViewport();
      }, 250);
    };
    window.addEventListener("resize", onViewportChange);

    // No devicePixelRatio watcher, and the desktop follows the host's density
    // anyway: the canvas is sized in the remote's points, so a window dragged
    // between monitors of different scale is re-rasterized at the new one by the
    // browser — a 1x guest magnifying on Retina, a Retina guest halving on a 1x
    // screen — with its physical size unchanged. A watcher was only needed back
    // when this size was in the host's device pixels and had to be recomputed.

    return () => {
      disposed = true;
      startRef.current = null;
      resizeToWindowRef.current = null;
      // The socket is going away, so nothing will answer a pending fetch.
      settleClipboardWaiters(null);
      clearTimeout(retryTimer);
      window.removeEventListener("resize", onViewportChange);
      clearTimeout(resizeTimer);
      ws?.close();
    };
  }, [canvasRef, onUnauthorized, syncCursor, settleClipboardWaiters]);

  // Force-claim the slot: the takeover confirmation (busy) and the take-back
  // action after being evicted (takenOver).
  const takeOver = useCallback(() => startRef.current?.(true), []);

  // Pick a target from the picker: start its session over the live socket. The
  // server answers `connected` (→ desktop) or `error` (shown on the picker).
  const connect = useCallback((target: string) => {
    setConnectError(null);
    setPendingTarget(target);
    sendRef.current({ type: "connect", target });
  }, []);

  // Switch target: tear the current session down and return to the picker. The
  // server answers `picker`, which flips `mode` back.
  const switchTarget = useCallback(() => {
    sendRef.current({ type: "disconnect" });
  }, []);

  // Resize the remote desktop to the current browser window (the floating
  // menu's "Resize to window", shown only when `canResize`). A no-op while the
  // socket is down. The engine answers with a `resize` control message.
  const resizeToWindow = useCallback(() => {
    resizeToWindowRef.current?.();
  }, []);

  // Share a different one of the remote's displays (the Display panel). Fire
  // and forget, and deliberately not optimistic: the answer is the remote's
  // next `displays`, which is what moves the checkmark. A no-op while the
  // socket is down.
  const selectDisplay = useCallback((id: number) => {
    sendRef.current({ type: "selectDisplay", id });
  }, []);

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

  // Ask the server for the remote's clipboard and wait for the answer, which
  // also lands in `remoteClipboard` on its way past. Resolves with the snapshot,
  // or `null` when nothing came back — the socket was down, the session
  // returned to the picker, or the engine never answered.
  //
  // Awaitable so the toolbar can hold the panel closed until there is something
  // current to show, rather than opening on stale text that updates a moment
  // later. Every engine answers exactly one `clipboard` message per request
  // (from a buffer for VNC and RDP, from a live pasteboard read for `rxa`), so
  // this behaves the same on all three.
  const requestClipboard =
    useCallback((): Promise<ClipboardSnapshot | null> => {
      const ws = wsRef.current;
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        return Promise.resolve(null);
      }
      ws.send(JSON.stringify({ type: "clipboardRequest" } satisfies ClientMsg));
      return new Promise((resolve) => {
        let timer: ReturnType<typeof setTimeout>;
        const waiter = (snapshot: ClipboardSnapshot | null) => {
          clearTimeout(timer);
          resolve(snapshot);
        };
        timer = setTimeout(() => {
          clipboardWaitersRef.current = clipboardWaitersRef.current.filter(
            (w) => w !== waiter,
          );
          resolve(null);
        }, CLIPBOARD_FETCH_TIMEOUT_MS);
        clipboardWaitersRef.current.push(waiter);
      });
    }, []);

  // Put `text` on the remote's clipboard. Fire and forget: neither VNC's
  // ClientCutText nor the agent's pasteboard write is acknowledged, so there is
  // nothing to await.
  const sendClipboard = useCallback((text: string) => {
    // Recorded even for a manual Send from the panel: some VNC servers echo a
    // cut straight back as ServerCutText, and without this the round trip
    // would overwrite the local clipboard with what we just sent.
    lastToRemoteRef.current = text;
    sendRef.current({ type: "clipboard", text });
  }, []);

  // Push the local clipboard to the remote whenever this tab becomes active,
  // so a paste inside the remote desktop — context menu, middle click, or a
  // Ctrl+V the remote app handles itself — sees what was last copied here.
  //
  // Focus is the trigger because reading the clipboard is only permitted for a
  // focused document, and it is also the moment the user has plausibly just
  // copied something elsewhere. Everything here is best effort: `readText` is
  // absent on a non-secure origin, and Safari refuses it outright without a
  // paste gesture. The panel's Send covers those.
  //
  // An oversized clipboard is skipped rather than sent for the gateway to
  // truncate: this fires on focus with no user action behind it, and the whole
  // string would ride the socket first — past 64 MiB (axum's default
  // max_message_size) that drops the session, which reconnect then hides. The
  // remote therefore keeps whatever it had; the panel's Send is the path that
  // reports the limit, since autosync has no UI to report it in.
  //
  // The inbound half (mirrorRemoteClipboard) needs no check: everything on that
  // link is already clamped by the gateway (see src/protocol.rs).
  useEffect(() => {
    if (mode !== "desktop" || !canClipboard) {
      return;
    }
    const pushLocalClipboard = () => {
      if (document.hidden || !document.hasFocus()) {
        return;
      }
      void (async () => {
        let text: string;
        try {
          text = (await navigator.clipboard?.readText?.()) ?? "";
        } catch {
          return; // no permission, no secure context, or the user declined
        }
        if (
          text === "" ||
          overClipboardLimit(text) ||
          // Came from the remote a moment ago; sending it back is a loop.
          text === lastFromRemoteRef.current ||
          text === lastToRemoteRef.current
        ) {
          return;
        }
        lastToRemoteRef.current = text;
        sendRef.current({ type: "clipboard", text });
      })();
    };
    window.addEventListener("focus", pushLocalClipboard);
    document.addEventListener("visibilitychange", pushLocalClipboard);
    // Also once now: the tab may already be focused when the session starts.
    pushLocalClipboard();
    return () => {
      window.removeEventListener("focus", pushLocalClipboard);
      document.removeEventListener("visibilitychange", pushLocalClipboard);
    };
  }, [mode, canClipboard]);

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
      syncCursor();
    },
    [canvasRef, syncCursor],
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
            syncCursor();
          },
          bottomInset: () => bottomInsetRef.current,
          // The virtual pointer moved: redraw it at its new spot. A hardware
          // mouse (`real`) needs no image — the CSS cursor already sits under
          // it — so it clears the position instead.
          onCursor: (x, y, real) => {
            touchCursorRef.current = real ? null : { x, y };
            syncCursor();
          },
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
  }, [overlayRef, canvasRef, syncCursor]);

  return {
    status,
    mode,
    connectError,
    pendingTarget,
    size,
    canResize,
    canClipboard,
    displays,
    activeDisplayId,
    remoteClipboard,
    takeOver,
    connect,
    switchTarget,
    resizeToWindow,
    selectDisplay,
    sendKeyCombo,
    requestClipboard,
    sendClipboard,
    setBottomInset,
  };
}
