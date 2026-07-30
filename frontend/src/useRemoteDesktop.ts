import { useCallback, useEffect, useRef, useState } from "react";
import {
  type AudioPlayer,
  audioUnavailable,
  createAudioContext,
  createAudioPlayer,
  decodeAudioHead,
} from "./audioPlayer.ts";
import { isMacHost, MacKeyboardTranslator } from "./macKeys.ts";
import { createSender } from "./outbound.ts";
import {
  type BatchRecord,
  binaryFrameKind,
  type ClientMsg,
  type ClipboardSnapshot,
  type ControlMsg,
  type DisplayInfo,
  decodeAudioFrame,
  decodeBatchFrame,
  MAX_CLIPBOARD_BYTES,
  type MouseButton,
  mouseButtonFromEvent,
  NO_SLOT,
  type RemoteClipboard,
  SLOT_COUNT,
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
  | "takenOver" // this socket was evicted by a takeover (close code 4001)
  // The session could not be opened for a reason waiting cannot change, so nothing
  // is in flight and nothing is scheduled. Its own state because the alternative was
  // leaving "Connecting…" up over a connection that had stopped being attempted,
  // with no way out but a reload; the overlay offers Retry on this one.
  | "failed";

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
// The Mac-host Command-to-Control preference, default on. localStorage rather
// than sessionStorage: unlike the session identity this is a lasting choice about
// how this machine's keyboard behaves, and it should survive a new tab.
const MAC_KEYS_KEY = "remotex.macKeyboardOverrides";
// Evaluated once: the host OS cannot change under a running tab, and the input
// effect must not pay for it per keystroke.
const IS_MAC_HOST = isMacHost();

// Whether Command chords should be translated for a non-Mac guest, as last set
// here. Absent means on, matching the viewer's default-on menu item.
function readMacKeyOverridesPreference(): boolean {
  try {
    return localStorage.getItem(MAC_KEYS_KEY) !== "off";
  } catch {
    return true; // storage disabled or blocked; the default is still the default
  }
}
// Touch clients keep a fixed guest size and use fit-to-width plus pinch zoom.
export const CAN_PINCH_ZOOM = (navigator.maxTouchPoints || 0) >= 2;

// Phone or tablet, off the screen's short side in CSS pixels. Deliberately the
// crudest test that separates them: the largest phone is around 440 CSS px across
// its short side and the smallest tablet around 740, so the boundary sits in a gap
// no real device occupies and nothing near it has an answer worth getting right.
const TABLET_MIN_SHORT_SIDE = 600;

// Tablets request their screen's landscape dimensions; phones request the
// target default. Screen dimensions keep rotation and browser chrome irrelevant.
const MOBILE_GUEST_SIZE: { w: number; h: number } | null = (() => {
  const long = Math.max(screen.width, screen.height);
  const short = Math.min(screen.width, screen.height);
  return short >= TABLET_MIN_SHORT_SIDE ? { w: long, h: short } : null;
})();

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
// How many failed attempts in a row are reported as nothing but "Reconnecting…"
// before the reason is shown as well. Four, because the backoff above reaches its
// cap at the fourth attempt: about half a minute, which is long enough that a
// gateway coming up would have come up, and short enough that nobody has gone to
// read DNS records yet. Matches `SessionStateMachine.attemptsBeforeReporting` in
// the macOS viewer, which answers the same complaint.
const ATTEMPTS_BEFORE_REPORTING = 4;

// How long `requestClipboard` waits for the server's answer before giving up.
// Generous because it is not all local: a VNC or RDP target answers from an
// engine-side buffer immediately, but an `rxa` target is a real round trip to
// the Mac, and one made during an agent reconnect is discarded outright and
// never answered at all.
const CLIPBOARD_FETCH_TIMEOUT_MS = 5000;

// Lay out the framebuffer at remote pixels / remote scale CSS pixels. Host
// devicePixelRatio is intentionally absent: browser rasterization handles the
// host display, while sendHostScale separately asks the remote to render at this
// screen's density (an owned RXA display, or an RDP host that allows resize).
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

// Scale a framebuffer-pixel cursor with image-set resolution. Keep a plain URL
// fallback because unsupported image-set values are rejected as a whole.
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

// Why an attempt to open the session did not open it, and whether waiting could
// change that.
//
// `retryable` is the whole distinction: a fetch that never got an answer could get
// one a second from now, while a 502, an answer that could not be read, or a
// refused request are facts that stand still. Retrying the second kind is how every
// failure came to be reported as "Reconnecting…" forever — see `scheduleRetry`.
type ClaimFailure = { reason: string; retryable: boolean };

// POST /api/session (the slot claim). A rejected fetch is the only retryable
// outcome, and its own message says what happened far better than "network error"
// would — including the cases that are not the network at all.
async function postClaim(
  force: boolean,
): Promise<Response | { failure: ClaimFailure }> {
  try {
    return await fetch("/api/session", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        force,
        sessionId: sessionStorage.getItem(SESSION_KEY) ?? undefined,
      }),
    });
  } catch (cause) {
    return {
      failure: {
        reason:
          cause instanceof Error ? cause.message : "the server did not answer",
        retryable: true,
      },
    };
  }
}

// This screen's density in hundredths, the unit the wire carries.
//
// Rounded because that wire is an integer, and defaulted rather than clamped: a
// fractional-DPI screen is ordinary, a `devicePixelRatio` of 0 or Infinity is not
// a screen at all, and 1x is the answer that asks the remote for the least.
function hostScaleHundredths(): number {
  const dpr = window.devicePixelRatio;
  return Number.isFinite(dpr) && dpr > 0 ? Math.round(dpr * 100) : 100;
}

// A density as a menu reads it: `2x`, `1x`, `1.5x` for the fractional screens
// that exist. Hundredths in, because that is what both ends of this speak.
export function densityLabel(hundredths: number): string {
  return `${Number((hundredths / 100).toFixed(2))}x`;
}

// Requested CSS viewport converted to remote pixels and clamped to wire `u16`.
function viewportMsg(
  size: { w: number; h: number },
  guestScale: number,
): Extract<ClientMsg, { type: "viewport" }> {
  const density = guestScale > 0 ? guestScale : 1;
  const dim = (cssPx: number) =>
    Math.min(65535, Math.max(1, Math.round(cssPx * density)));
  return { type: "viewport", w: dim(size.w), h: dim(size.h) };
}

// Own the single-session claim, WebSocket lifecycle, picker/desktop state,
// rendering, and input forwarding. `onUnauthorized` must be referentially stable
// because it participates in the connection effects.
export function useRemoteDesktop(
  canvasRef: React.RefObject<HTMLCanvasElement | null>,
  overlayRef: React.RefObject<HTMLElement | null>,
  pointerRef: React.RefObject<HTMLImageElement | null>,
  onUnauthorized: () => void,
) {
  const [status, setStatus] = useState<ConnectionStatus>("connecting");
  const [size, setSize] = useState<RemoteSize | null>(null);
  // This screen's density, kept in state only so the menu can show it beside the
  // remote's. Nothing about how the desktop is presented reads it — see
  // applyCanvasCss. Seeded from the screen rather than left null, so the readout
  // is right from the first paint instead of after the first send.
  const [hostScale, setHostScale] = useState(hostScaleHundredths);
  // Picker vs desktop. `connectError` holds the last engine error to show
  // against the picker after a failed connect.
  const [mode, setMode] = useState<SessionMode>("picker");
  const [connectError, setConnectError] = useState<string | null>(null);
  // The remote's own session slot is held by a different client, and this one can
  // take it over. Beside `connectError` rather than folded into it because the
  // picker offers a button for this and only reports the other. Cleared the moment
  // a session starts or another connect is attempted, so a stale offer can never
  // outlive the situation that produced it (see the `remoteBusy` control message).
  const [remoteBusy, setRemoteBusy] = useState<{
    target: string;
    holder: string;
    heldSecs: number;
  } | null>(null);
  // The target a connect() is waiting on, so the picker can show progress
  // until the server answers with `connected` (or an error).
  const [pendingTarget, setPendingTarget] = useState<string | null>(null);
  // True when the connected target supports resize but only on request (RDP):
  // the floating menu shows a "Resize to window" button and automatic viewport
  // reports are suppressed. VNC resizes automatically, so it stays false — and so
  // does every case on a pinch-zoom device, where the window this would resize to
  // is not one to hand a remote desktop (see CAN_PINCH_ZOOM).
  const [canResize, setCanResize] = useState(false);
  // True when the connected target opted into the clipboard bridge, which is
  // what enables the floating menu's Clipboard button.
  const [canClipboard, setCanClipboard] = useState(false);
  // Whether this target offers remote audio; this says nothing about activity.
  const [canAudio, setCanAudio] = useState(false);
  // Whether this browser has asked for the sound. Per attachment and never
  // remembered: it starts off on every connect and reconnect, because enabling it
  // has to happen inside a click — that is what makes an AudioContext playable
  // without an autoplay policy's permission.
  const [audioEnabled, setAudioEnabled] = useState(false);
  // Why there is no sound, when there should be. One string with one real cause
  // behind it today: a browser with no WebCodecs Opus decoder, which is a plain
  // refusal rather than something to work around — there is no second
  // representation to fall back to (see audioPlayer.ts).
  const [audioError, setAudioError] = useState<string | null>(null);
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
  // Whether the remote reported itself as a Mac, which is the only thing that
  // decides whether a local Command chord stays Command or becomes remote
  // Control. Reset to false on every disconnect (see the "picker" case), because
  // a target that never reports makes the question moot and the last answer must
  // not carry over to it.
  const [remoteIsMac, setRemoteIsMac] = useState(false);
  const [macKeyOverridesEnabled, setMacKeyOverridesEnabled] = useState(
    readMacKeyOverridesPreference,
  );
  // All three conditions, which the toolbar shows and the input effect obeys: a
  // Mac keyboard to translate from, a guest that is not a Mac to translate for,
  // and the user's consent. Off on a non-Mac host means the physical `code` goes
  // out untouched, which is what this client has always done.
  const macKeyOverridesActive =
    IS_MAC_HOST && macKeyOverridesEnabled && !remoteIsMac;
  // Read by the key handlers, which must not re-subscribe when it changes: a
  // teardown mid-chord would strand whatever is held.
  const macKeyOverridesActiveRef = useRef(macKeyOverridesActive);
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

  useEffect(() => {
    if (macKeyOverridesActiveRef.current === macKeyOverridesActive) {
      return;
    }
    macKeyOverridesActiveRef.current = macKeyOverridesActive;
    releaseKeysRef.current?.();
  }, [macKeyOverridesActive]);

  // Persist the preference as it changes rather than in the toggle callback, so
  // the stored value follows the state whatever set it.
  useEffect(() => {
    try {
      localStorage.setItem(MAC_KEYS_KEY, macKeyOverridesEnabled ? "on" : "off");
    } catch {
      // Storage blocked: the preference still holds for this tab.
    }
  }, [macKeyOverridesEnabled]);

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
  // Releases every key the input effect has sent as pressed and clears the
  // Command translator with them. Set by that effect; called from here when the
  // translation rules change under a chord that is part way through — the guest
  // has already been told those keys are down, and the new rules would send the
  // releases for different codes.
  //
  // Keys only, deliberately: the mouse and the gesture layer have nothing to do
  // with which chord a Command means, and a held button or an in-flight pinch is
  // not the toolbar's business to cancel. Focus loss is the case that releases
  // everything.
  const releaseKeysRef = useRef<(() => void) | null>(null);
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
  // Which target this session is for, so a `remoteBusy` can name the target its
  // takeover would apply to. A ref because the control handler reads it without
  // wanting to be re-created when it changes, and because the message can arrive
  // in two states: while a pick is pending (the ordinary refusal) and mid-session,
  // when another client took the remote during a reconnect and `pendingTarget` is
  // long since null.
  const sessionTargetRef = useRef<string | null>(null);
  // Manual-resize mode (RDP with resize enabled, rxa, and every mobile session):
  // while set, automatic viewport reports are suppressed and only the menu's
  // "Resize to window" sends one — which on mobile is nothing, since the button is
  // not offered there.
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

  // The single choke point for everything sent to the server, including the
  // touch gesture layer's synthesized events. Pointer motion is coalesced here
  // while the socket is backed up; see `createSender`.
  const sendRef = useRef(createSender(() => wsRef.current));

  // The audio context, from the click that enabled audio until the decoder is built
  // around it, and null after that — the player owns it from then on. Two refs
  // rather than one because they are created a round trip apart and for different
  // reasons: the context has to be born inside the gesture, and the decoder cannot
  // exist until `audioFormat` says what to decode.
  const audioContextRef = useRef<AudioContext | null>(null);
  const audioPlayerRef = useRef<AudioPlayer | null>(null);

  // Give up the audio hardware, telling the server nothing.
  //
  // Separate from the toggle because most of the ways audio ends are not the user
  // deciding: the target disconnected, the socket dropped, another browser took the
  // session. In all of those the gateway has already stopped, so a message back
  // would be answering a question nobody asked — and on a closed socket it would go
  // nowhere anyway.
  const releaseAudio = useCallback(() => {
    // Exactly one of these holds the context, which is what keeps this from calling
    // `close()` on an already-closed one.
    audioPlayerRef.current?.close();
    audioPlayerRef.current = null;
    void audioContextRef.current?.close();
    audioContextRef.current = null;
  }, []);

  // The connection driver: claim -> WebSocket -> render, with auto-reconnect.
  useEffect(() => {
    ctxRef.current = canvasRef.current?.getContext("2d") ?? null;

    let disposed = false;
    let ws: WebSocket | null = null;
    // The tiles the server has told this client to remember, by slot. Fixed
    // length because the wire says how many there are (`SLOT_COUNT`), so a server
    // cannot grow it — and the client never evicts: the server names the slot to
    // overwrite, which is what keeps the two ends in step without either
    // modelling the other's memory.
    const tileCache: ({ data: Uint8Array; mime: TileMsg["mime"] } | null)[] =
      new Array(SLOT_COUNT).fill(null);
    // Whether a reset has already been asked for while handling this batch.
    let resetAsked = false;
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
      // The next attachment's server starts with an empty slot table, so holding
      // these would only cost memory. (Nothing could be *drawn* wrongly: a
      // reference always follows the tile that filled its slot on the same
      // socket.)
      tileCache.fill(null);
      // The socket carrying the audio is going away, and a subscription belongs to
      // one attachment: the gateway has already stopped this one, so holding a
      // decoder open would only be holding the audio hardware. The next `connected`
      // starts from off, and getting it back is a click (see `setAudio`).
      releaseAudio();
      setAudioEnabled(false);
      syncCursor();
    };

    // `reason` is what went wrong, when the caller knows: shown once the retries
    // have stopped explaining themselves (see `ATTEMPTS_BEFORE_REPORTING`), and the
    // retries carry on either way — a laptop that was asleep for ten minutes still
    // has to recover by itself, which is what retrying forever is for. What was
    // wrong was never the retrying; it was that "Reconnecting…" was the only thing
    // anybody was ever told, so a server answering 502 and a slow network looked
    // identical for as long as you cared to watch.
    const scheduleRetry = (reason?: string) => {
      if (disposed) {
        return;
      }
      clearDesktop();
      setStatus("reconnecting");
      if (reason && attempts >= ATTEMPTS_BEFORE_REPORTING) {
        setConnectError(reason);
      }
      const delay = Math.min(1000 * 2 ** attempts, MAX_RETRY_DELAY_MS);
      attempts += 1;
      retryTimer = setTimeout(() => void connect(false), delay);
    };

    // Claim the session slot. Returns the token, "busy" when another browser
    // holds the slot (409), "unauthorized" when the login is gone (401), or the
    // reason it failed — which the caller reports rather than swallowing.
    const claim = async (
      force: boolean,
    ): Promise<string | "busy" | "unauthorized" | ClaimFailure> => {
      const res = await postClaim(force);
      if ("failure" in res) {
        return res.failure;
      }
      if (res.status === 409) {
        return "busy";
      }
      if (res.status === 401) {
        return "unauthorized";
      }
      // Not retryable, and this is the case that hurt most: a gateway answering
      // 502 or 500 was reported as a connection problem forever, with the status
      // code visible nowhere.
      if (!res.ok) {
        return {
          reason: `the server answered ${res.status}`,
          retryable: false,
        };
      }
      try {
        const { sessionId } = (await res.json()) as { sessionId: string };
        return sessionId;
      } catch {
        return {
          reason: "the server's answer could not be read",
          retryable: false,
        };
      }
    };

    // An attempt that did not open a session. One place for it, so the rule about
    // what is worth waiting for is written once: a failure that could pass is
    // retried, and one the server has already decided is said and left alone —
    // retrying that is what made a definite answer look like weather.
    const failed = (failure: ClaimFailure) => {
      if (failure.retryable) {
        scheduleRetry(failure.reason);
        return;
      }
      clearDesktop();
      setConnectError(failure.reason);
      // Not left as "connecting"/"reconnecting": nothing is, and saying so is the
      // whole point — the reason is shown beside a Retry button instead of under a
      // status that promises an attempt nobody is making.
      setStatus("failed");
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
        // Whatever the last attempt failed with is not why this one stopped: the slot
        // is simply somebody else's, which the status says on its own.
        setConnectError(null);
        setStatus("busy");
        return;
      }
      if (claimed === "unauthorized") {
        onUnauthorized(); // unmounts this hook's component
        return;
      }
      if (typeof claimed !== "string") {
        failed(claimed);
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
      const el = document.documentElement;
      const msg = viewportMsg(
        { w: el.clientWidth, h: el.clientHeight },
        sizeRef.current?.scale ?? 1,
      );
      if (
        lastViewport &&
        lastViewport.w === msg.w &&
        lastViewport.h === msg.h
      ) {
        return;
      }
      lastViewport = { w: msg.w, h: msg.h };
      sendRef.current(msg);
    };
    // Send one mobile size after the first resize supplies the remote density.
    // Rotations and browser chrome do not revise it.
    let mobileSizePending = false;
    const sendMobileSize = () => {
      mobileSizePending = false;
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        return;
      }
      sendRef.current(
        MOBILE_GUEST_SIZE
          ? viewportMsg(MOBILE_GUEST_SIZE, sizeRef.current?.scale ?? 1)
          : { type: "defaultSize" },
      );
    };
    // The manual "Resize to window" action: report the viewport even in
    // manual-resize mode. Dedup still applies, so re-clicking at the same
    // window size won't fire a redundant resize.
    resizeToWindowRef.current = () => sendViewport({ manual: true });

    // This screen's density, deduped the same way. Two kinds of target act on it,
    // and both by matching it: an rxa target with a display the agent made, and an
    // RDP target that allows resize. Re-sending an unchanged value would be a
    // WindowServer round trip or a full RDP reactivation for nothing.
    let lastHostScale: number | null = null;
    // Which display the remote is sharing, as its last `displays` reported it.
    // Only so a switch can be told from the first list of a session.
    let sharedDisplay: number | null = null;
    // RXA resize requires both target permission and an active owned display.
    let rxaResize = false;
    const sendHostScale = () => {
      const scale = hostScaleHundredths();
      // Recorded before either guard below, and whether or not it is sent: the
      // menu shows this number, and a density this screen has that the remote
      // does not is exactly what someone reading it is trying to see.
      setHostScale(scale);
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        return;
      }
      if (lastHostScale === scale) {
        return;
      }
      lastHostScale = scale;
      sendRef.current({ type: "hostScale", scale });
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
      // Tiles decode asynchronously (createImageBitmap), so all messages are
      // chained through one promise queue: draws land in arrival order (later
      // tiles must overwrite earlier ones) and a resize can't jump the queue.
      // The catch keeps a garbled frame from stalling the chain.
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
      if (!(data instanceof ArrayBuffer)) {
        return;
      }
      // Read the kind before either parser: a batch parser handed audio would spend
      // its way through an Opus packet looking for tile records.
      switch (binaryFrameKind(data)) {
        case "batch":
          await drawBatch(data);
          break;
        case "audio":
          playAudio(data);
          break;
        default:
          break;
      }
    };

    // Audio rides the same promise queue as tiles, which is worth a word because it
    // sounds wrong: a decode that awaited behind a repaint would be exactly the
    // head-of-line delay this design is meant to avoid. It does not await — the
    // packets are handed to WebCodecs, which decodes off-thread and calls back — so
    // what queues here is a few microseconds of copying, not a decode.
    const playAudio = (data: ArrayBuffer) => {
      const packets = decodeAudioFrame(data);
      if (packets) {
        audioPlayerRef.current?.push(packets);
      }
    };

    // Build the decoder the format describes, around the context the click made.
    //
    // Arriving without a context means audio was turned off between the request and
    // this answer, or that the gateway sent it unasked; either way there is nothing
    // to build and nothing to report.
    const startAudio = (msg: Extract<ControlMsg, { type: "audioFormat" }>) => {
      const context = audioContextRef.current;
      if (!context) {
        return;
      }
      try {
        const player = createAudioPlayer(
          {
            codec: msg.codec,
            sampleRate: msg.sampleRate,
            channels: msg.channels,
            head: decodeAudioHead(msg.head),
          },
          context,
          {
            onError: (reason) => {
              releaseAudio();
              setAudioEnabled(false);
              setAudioError(reason);
              // And stop the packets, which would otherwise keep arriving for a
              // decoder that has gone.
              sendRef.current({ type: "audio", enabled: false });
            },
            // Only the trims, and they earn a warning: audio was thrown away to
            // stay near live, which is the ceiling doing its job and also the one
            // event in this path that should be rare. A steady-state lead needs no
            // logging — it cannot leave the range the schedule defines (see
            // audioSchedule.ts), which is the point: if the sound is still late with
            // no trims recurring, the delay is upstream of this browser.
            onLead: (lead, trimmed) => {
              if (trimmed > 0) {
                console.warn(
                  `audio: trimmed ${trimmed.toFixed(3)}s to stay near live (lead ${lead.toFixed(3)}s)`,
                );
              }
            },
          },
        );
        audioPlayerRef.current = player;
        // The player owns the context now, so this must not also close it.
        audioContextRef.current = null;
        setAudioError(null);
      } catch (e) {
        releaseAudio();
        setAudioEnabled(false);
        setAudioError(
          e instanceof Error
            ? e.message
            : "this browser cannot play remote audio",
        );
        sendRef.current({ type: "audio", enabled: false });
      }
    };

    // Decode records concurrently, then draw synchronously in wire order.
    // Malformed framing drops the batch; individual decode failures drop a tile.
    const drawBatch = async (data: ArrayBuffer) => {
      const records = decodeBatchFrame(data);
      if (!records) {
        return;
      }
      // One reset per batch at most: fifty references into a cache this client
      // lost are one disagreement, not fifty.
      resetAsked = false;
      const jobs = records.map(resolveRecord);
      paintBatch(jobs, await Promise.all(jobs.map(decodeJob)));
      // Clear after the pass so references may use slots filled earlier in it.
      if (resetAsked) {
        tileCache.fill(null);
      }
    };

    // What a record turns into once the cache has had its say: a payload to
    // decode and where to put it, or nothing.
    interface PaintJob {
      x: number;
      y: number;
      data: Uint8Array;
      mime: TileMsg["mime"];
      /** True when the server believes this client is keeping these bytes. */
      cached: boolean;
    }

    // Store what the server says to store, and resolve what it says to reuse.
    //
    // The payload is copied out of the frame rather than held as a view of it:
    // a view would pin the whole batch — up to 256 KB — for the lifetime of one
    // slot.
    const resolveRecord = (record: BatchRecord): PaintJob | null => {
      if (record.kind === "tile") {
        if (record.slot !== NO_SLOT) {
          tileCache[record.slot] = {
            data: new Uint8Array(record.data),
            mime: record.mime,
          };
        }
        return { ...record, cached: record.slot !== NO_SLOT };
      }
      const held = tileCache[record.slot];
      if (!held) {
        // The server thinks this client holds a tile it does not. Nothing else
        // will ever correct that, so say so and draw nothing here.
        askForCacheReset();
        return null;
      }
      return { x: record.x, y: record.y, ...held, cached: true };
    };

    const decodeJob = async (job: PaintJob | null) => {
      if (!job) {
        return null;
      }
      try {
        return await createImageBitmap(
          new Blob([job.data as Uint8Array<ArrayBuffer>], { type: job.mime }),
        );
      } catch {
        // A tile that will not decode is one dropped tile — unless the server is
        // keeping it as a slot, in which case every later reference to it would
        // fail the same way.
        if (job.cached) {
          askForCacheReset();
        }
        return null;
      }
    };

    const askForCacheReset = () => {
      if (resetAsked) {
        return;
      }
      resetAsked = true;
      sendRef.current({ type: "cacheReset" });
    };

    // Every bitmap is closed whether or not it was drawn: with no canvas to draw
    // into there is nothing to paint, but the decoded images still have to go.
    const paintBatch = (
      jobs: (PaintJob | null)[],
      bitmaps: (ImageBitmap | null)[],
    ) => {
      const ctx = ctxRef.current;
      for (let i = 0; i < bitmaps.length; i += 1) {
        const bitmap = bitmaps[i];
        const job = jobs[i];
        if (!bitmap || !job) {
          continue;
        }
        ctx?.drawImage(bitmap, job.x, job.y);
        bitmap.close();
      }
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
      // The mobile size request, now that there is a density to convert with —
      // see sendMobileSize. Fired from the *first* resize of a connection only, so
      // the answer this produces does not immediately re-trigger it.
      if (mobileSizePending) {
        sendMobileSize();
      }
    };

    // The remote's display list, and the one follow-up a change of display
    // needs: this screen's density again.
    //
    // The number has not changed, but what it applies to has. The agent sets the
    // density of whichever display it is sharing *now*, so a switch onto one it
    // made would otherwise leave that display at whatever density macOS had
    // remembered against it. Only a real switch re-reports: the first list of a
    // session names the display the report from `connected` already applied to.
    const handleDisplays = (msg: Extract<ControlMsg, { type: "displays" }>) => {
      setDisplays(msg.displays);
      setActiveDisplayId(msg.active);
      // Whether "Resize to window" is offered is a question about *this*
      // display, not only about the target: only a display the agent made can be
      // resized from here, and the user can switch onto and off one mid-session
      // from the Display panel. Read off the message rather than the `displays`
      // state set a line above, which this render has not seen yet. An `active`
      // the list does not name — a screen unplugged between the two, which this
      // message allows — reads as not virtual, so the button disappears rather
      // than offering to resize a display nobody here can identify.
      if (rxaResize) {
        const active = msg.displays.find(
          (display) => display.id === msg.active,
        );
        setCanResize(active?.virtual === true);
      }
      const switched = sharedDisplay !== null && sharedDisplay !== msg.active;
      sharedDisplay = msg.active;
      if (switched) {
        lastHostScale = null;
        sendHostScale();
      }
    };

    const handleConnected = (
      msg: Extract<ControlMsg, { type: "connected" }>,
    ) => {
      // A target session started (picker connect, reattach, or takeover of a
      // live desktop): switch to the desktop.
      setConnectError(null);
      setRemoteBusy(null);
      setPendingTarget(null);
      sessionTargetRef.current = msg.name;
      setMode("desktop");
      setCanClipboard(msg.clipboard);
      setCanAudio(msg.audio);
      // Audio starts off on every `connected`, reattach and takeover included, and
      // that is not a reset for tidiness: a subscription belongs to one attachment,
      // so the gateway is not sending any, and asking again has to come from a click
      // for the AudioContext to be allowed to play. Whatever was playing belonged to
      // a socket that is gone.
      releaseAudio();
      setAudioEnabled(false);
      setAudioError(null);
      if (CAN_PINCH_ZOOM) {
        // Mobile has one rule and it does not vary by protocol: ask once, here,
        // and never let this window's shape reach the remote again. So every
        // automatic sender is suppressed and "Resize to window" is never offered —
        // the window it would resize to is the one this client deliberately does
        // not ask the remote to be.
        //
        // Gated on the target's `resize` because there is nothing to say
        // otherwise: an engine drops both requests without it, and this keeps the
        // browser from asking for something the operator declined.
        manualResizeRef.current = true;
        rxaResize = false;
        setCanResize(false);
        mobileSizePending = msg.resize;
      } else {
        // Three behaviours, and only two of them are settled here. VNC follows the
        // viewport automatically. RDP resizes only when asked, because its
        // reactivation is heavy — so viewport reports are suppressed and the menu's
        // "Resize to window" is the one caller. rxa is only-when-asked too, but
        // *whether it may be asked* is a fact about the display being shared rather
        // than about the target, so it is settled in `handleDisplays`: a Mac's own
        // panel is never resized because somebody connected, and only a display the
        // agent made for the purpose can be.
        const manual = msg.protocol === "rdp" && msg.resize;
        rxaResize = msg.protocol === "rxa" && msg.resize;
        // Suppressing the automatic senders is unconditional for rxa, whatever the
        // target allows: even a display made to be looked at from here is not
        // dragged around by this window, and the report three lines down is one of
        // the sends this has to stop.
        manualResizeRef.current = manual || msg.protocol === "rxa";
        // For rxa this starts false and stays false until the first `displays`.
        setCanResize(manual);
        // A freshly-started engine needs the current viewport; the report is sent
        // here (once the protocol is known), undeduped.
        lastViewport = null;
        sendViewport();
      }
      // And this screen's density, which is what lets a display the agent made
      // come up matching the window it is about to be shown in rather than at
      // whatever density it was left at.
      lastHostScale = null;
      sharedDisplay = null;
      sendHostScale();
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
        case "remoteBusy":
          // Not an error: the remote answered correctly and named who has it. The
          // session ends the same way, back on the picker, but with a takeover to
          // offer against the target instead of a message to read.
          setConnectError(null);
          setRemoteBusy({
            target: sessionTargetRef.current ?? "",
            holder: msg.holder,
            heldSecs: msg.heldSecs,
          });
          setPendingTarget(null);
          break;
        case "connected":
          handleConnected(msg);
          break;
        case "audioFormat":
          startAudio(msg);
          break;
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
          handleDisplays(msg);
          break;
        case "remoteOs":
          // Decides one keyboard convention: does a local Command shortcut stay
          // Command or become remote Control. Only meaningful on a Mac host, and
          // only for the eight chords in macKeys.ts — the browser reserves the
          // rest for itself.
          setRemoteIsMac(msg.macos);
          break;
        case "picker":
          // No target selected (idle attach, switch-target, or an engine that
          // ended): show the picker. Drop any retained framebuffer so a later
          // connect starts from a clean "waiting for the desktop" state.
          setPendingTarget(null);
          setMode("picker");
          manualResizeRef.current = false;
          rxaResize = false;
          setCanResize(false);
          setCanClipboard(false);
          // No engine, so no queue to subscribe to: the row goes away rather than
          // offering a control that would be answered with a warning in the log.
          setCanAudio(false);
          releaseAudio();
          setAudioEnabled(false);
          setAudioError(null);
          // Back to the default rather than left as the last target's answer: the
          // next one may not report at all, and inheriting "the remote is a Mac"
          // would silently stop translating Command for a Windows guest.
          setRemoteIsMac(false);
          setDisplays([]);
          setActiveDisplayId(null);
          sharedDisplay = null;
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
      // The budget refills and last time's reason goes with it: it belonged to the
      // attempt the user just replaced, and leaving it up would blame this attempt
      // for the previous one's failure.
      setConnectError(null);
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

    // A devicePixelRatio watcher, but not for the canvas: nothing about how the
    // desktop is *presented* depends on this screen's density. The canvas is
    // sized in the remote's points, so a window dragged between monitors of
    // different scale is re-rasterized at the new one by the browser — a 1x guest
    // magnifying on Retina, a Retina guest halving on a 1x screen — with its
    // physical size unchanged and nothing here recomputed.
    //
    // What the new density is worth telling is the *remote*, so a display the
    // agent made — or an RDP host that allows resize — can match it and the
    // picture becomes one pixel per pixel instead of resampled. On RDP that also
    // moves the host's own UI scaling, which is the whole point: twice the pixels
    // with the same UI in them would only be a sharper version of too small.
    // There is no devicePixelRatio event, so this is the standard
    // trick: a media query pinned to the current ratio, which stops matching the
    // moment the ratio changes, re-armed each time from the new value.
    let dprQuery: MediaQueryList | null = null;
    const watchDpr = () => {
      dprQuery?.removeEventListener("change", onDprChange);
      dprQuery = window.matchMedia(
        `(resolution: ${window.devicePixelRatio}dppx)`,
      );
      dprQuery.addEventListener("change", onDprChange);
    };
    function onDprChange() {
      sendHostScale();
      watchDpr();
    }
    watchDpr();

    return () => {
      disposed = true;
      startRef.current = null;
      resizeToWindowRef.current = null;
      // The socket is going away, so nothing will answer a pending fetch.
      settleClipboardWaiters(null);
      clearTimeout(retryTimer);
      window.removeEventListener("resize", onViewportChange);
      dprQuery?.removeEventListener("change", onDprChange);
      clearTimeout(resizeTimer);
      ws?.close();
      releaseAudio();
    };
  }, [
    canvasRef,
    onUnauthorized,
    syncCursor,
    settleClipboardWaiters,
    releaseAudio,
  ]);

  // Force-claim the slot: the takeover confirmation (busy) and the take-back
  // action after being evicted (takenOver).
  const takeOver = useCallback(() => startRef.current?.(true), []);
  /// Try again after a failure that stopped the retries. Unforced, unlike
  /// `takeOver`: nothing here is holding the slot, so there is nobody to evict.
  const retry = useCallback(() => startRef.current?.(false), []);

  // Pick a target from the picker: start its session over the live socket. The
  // server answers `connected` (→ desktop), `error`, or `remoteBusy` (both shown
  // on the picker).
  //
  // `force` takes the *remote's* session slot from whoever holds it, and answers
  // the `remoteBusy` this picker just showed. It says nothing about this gateway's
  // own slot, which was claimed over HTTP before this socket existed — that
  // takeover is `takeOver()`.
  const connect = useCallback((target: string, force = false) => {
    setConnectError(null);
    setRemoteBusy(null);
    setPendingTarget(target);
    sessionTargetRef.current = target;
    sendRef.current({ type: "connect", target, force });
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

  // Start or stop the remote's sound (the floating menu's Audio button).
  //
  // **Must be called from a click**, and the AudioContext is why: a context created
  // inside a user gesture may play, and one created outside it is suspended on iOS
  // Safari with no way back. The decoder cannot be built here — `audioFormat` has not
  // arrived yet — so the context is what the gesture is spent on, and `startAudio`
  // wraps a decoder around it a round trip later.
  //
  // Optimistic, unlike `selectDisplay` next door: nothing acknowledges this, and the
  // honest reading of "enabled" is that this browser asked and is holding a context
  // open for the answer. A gateway that has nothing to send simply sends nothing.
  const setAudio = useCallback(
    (enabled: boolean) => {
      setAudioError(null);
      setAudioEnabled(enabled);
      releaseAudio();
      if (enabled) {
        // Said before the round trip rather than after it: there is no fallback
        // representation, so nothing about the answer would change this. And it
        // names which of the two reasons applies — a browser with no decoder, or an
        // insecure origin, where WebCodecs does not exist however capable the
        // browser is (see audioUnavailable).
        const unavailable = audioUnavailable();
        if (unavailable) {
          setAudioEnabled(false);
          setAudioError(unavailable);
          return;
        }
        audioContextRef.current = createAudioContext();
      }
      sendRef.current({ type: "audio", enabled });
    },
    [releaseAudio],
  );

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
      sendRef.current({ type: "clipboardRequest" });
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

  // Best-effort clipboard push on focus, when reads are permitted. Oversized
  // values are skipped locally; the explicit panel reports the limit.
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
    // Codes as *sent*, which is not the same as the codes typed: a translated
    // Command chord sends ControlLeft and swallows Meta, so releasing what was
    // typed would leave the guest holding a Control it was never told about.
    const pressedKeys = new Set<string>();
    const macKeys = new MacKeyboardTranslator();

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
    const releaseKeys = () => {
      for (const code of pressedKeys) {
        // caps is irrelevant on release: the backend releases the keysym it
        // recorded at press time.
        send({ type: "key", code, pressed: false, caps: false });
      }
      pressedKeys.clear();
      // After the releases, not before: the translator holds no state the sweep
      // above needs, and resetting it first would say a chord is over while its
      // codes are still going out.
      macKeys.reset();
    };
    const releaseAll = () => {
      releaseKeys();
      for (const button of pressedButtons) {
        send({ type: "mouseButton", button, pressed: false });
      }
      pressedButtons.clear();
      gestures?.release();
    };
    // Every key event goes through the Command translator, which is a
    // pass-through unless this is a Mac host driving a non-Mac guest. `pressedKeys`
    // follows what it emits rather than what arrived, so releaseAll can undo a
    // chord the guest was told about in different codes than the user typed.
    const sendTranslated = (e: KeyboardEvent, pressed: boolean) => {
      e.preventDefault();
      const caps = e.getModifierState("CapsLock");
      const translated = macKeys.translate(
        { code: e.code, pressed, caps, meta: e.metaKey },
        macKeyOverridesActiveRef.current,
      );
      for (const key of translated) {
        if (key.pressed) {
          pressedKeys.add(key.code);
        } else {
          pressedKeys.delete(key.code);
        }
        send({ type: "key", ...key });
      }
    };
    const onKeyDown = (e: KeyboardEvent) => sendTranslated(e, true);
    const onKeyUp = (e: KeyboardEvent) => sendTranslated(e, false);
    const onBlur = () => releaseAll();
    releaseKeysRef.current = releaseKeys;

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
      releaseKeysRef.current = null;
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
    remoteBusy,
    pendingTarget,
    size,
    hostScale,
    canResize,
    canClipboard,
    canAudio,
    audioEnabled,
    audioError,
    displays,
    activeDisplayId,
    remoteClipboard,
    // The preference and the three-way verdict, which the toolbar shows
    // separately: a Mac guest or a non-Mac host makes translation inapplicable
    // without the user having turned anything off.
    macKeyOverridesEnabled,
    macKeyOverridesActive,
    isMacHost: IS_MAC_HOST,
    remoteIsMac,
    setMacKeyOverridesEnabled,
    takeOver,
    retry,
    connect,
    switchTarget,
    resizeToWindow,
    selectDisplay,
    setAudio,
    sendKeyCombo,
    requestClipboard,
    sendClipboard,
    setBottomInset,
  };
}
