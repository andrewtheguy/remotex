import { useCallback, useEffect, useRef, useState } from "react";
import {
  type AudioPlayer,
  createAudioContext,
  createAudioPlayer,
  decodeAudioHead,
} from "./audioPlayer.ts";
import { postToCompanion, useCompanion } from "./companion.ts";
import { connectionLabel } from "./connectionLabel.ts";
import {
  applyCursorCss,
  cursorImage,
  MIN_POINTER_CSS_PX,
  type RemoteCursor,
} from "./cursorCss.ts";
import { desktopCanvasGeometry } from "./desktopCanvas.ts";
import { desktopPainterFor } from "./desktopPainter.ts";
import { gatewayFetch, gatewaySocketUrl } from "./gateway.ts";
import { keyboardLockHeld, onKeyboardLockChange } from "./immersive.ts";
import { isMacHost, MacKeyboardTranslator } from "./macKeys.ts";
import { NATIVE_HOST, postToHost } from "./nativeHost.ts";
import { createSender } from "./outbound.ts";
import { advancePaintGeneration, sendPaintAck } from "./paintAck.ts";
import { createRectCache } from "./pointerRect.ts";
import {
  binaryFrameKind,
  type ClientMsg,
  type ClipboardSnapshot,
  type ControlMsg,
  clickCount,
  type DisplayInfo,
  decodeAudioFrame,
  MAX_CLIPBOARD_BYTES,
  type MouseButton,
  mouseButtonFromEvent,
  type RemoteClipboard,
  wheelUnitFromEvent,
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
// The "sound by default" preference, off unless set — remembered like the
// Mac-keys one, and for the same reason: a lasting choice, not per-tab session
// state. Applied to a new connection only where the target carries audio, and
// toggling the live control in the desktop menu writes the same value back, so
// there is one setting with two places to set it.
const AUDIO_KEY = "remotex.audioByDefault";
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
// Both default off — an unset key is a target the user has not asked to reshape
// or to hear, which is the safe reading of silence for either. So `=== "on"`,
// where the Mac-keys default-on reader above is `!== "off"`.
function readOnByKey(key: string): boolean {
  try {
    return localStorage.getItem(key) === "on";
  } catch {
    return false; // storage disabled or blocked; the default is still the default
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
// read DNS records yet.
const ATTEMPTS_BEFORE_REPORTING = 4;

// How long `requestClipboard` waits for the server's answer before giving up.
// Generous rather than tight: the engines answer from an engine-side buffer, but
// a slow link or a fetch made mid-reconnect can still leave a request unanswered.
const CLIPBOARD_FETCH_TIMEOUT_MS = 5000;

// One instance for the one desktop canvas a page has: `applyCanvasCss` is a
// module-level function, and this is how it reaches the cache the pointer
// mapping reads. See pointerRect.ts for why invalidation has two triggers.
const pointerRectCache = createRectCache((clear) =>
  requestAnimationFrame(clear),
);

// The canvas bitmap remains the full remote framebuffer; only its CSS box is
// sized here, in the remote's own points. This is the same high-density canvas
// split used by ordinary DPR-aware renderers, except the guest has already drawn
// the high-density pixels, so the 2D context needs no scale transform.
function applyCanvasCss(
  canvas: HTMLCanvasElement | null,
  size: RemoteSize | null,
  view: TouchViewState,
  bottomInset = 0,
): void {
  if (!canvas || !size) {
    return;
  }
  // Every write below moves or resizes the canvas box, and a pointer event in
  // the same frame must not map through the box it replaced.
  pointerRectCache.invalidate();
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
  let { w, h } = desktopCanvasGeometry(size, size.scale).layout;
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
  // Through the shared rect cache: this is the same measurement the pointer
  // mapping needs, and a gesture frame that wrote canvas styles pays for at
  // most one layout flush between the two of them.
  const rect = els.canvas ? pointerRectCache.read(els.canvas) : undefined;
  // The desktop's on-screen scale, framebuffer pixels to CSS pixels: both
  // pointers are sized through it. 1:1 until the first resize names a size.
  const view = rect && size && size.w > 0 ? rect.width / size.w : 1;
  // What one cursor-image pixel covers on screen. A point-sized image (Apple's
  // density-independent pixmaps) follows the desktop's points, so it keeps its
  // size when the framebuffer goes Retina; a framebuffer-pixel image follows
  // the pixels it was cut from.
  const imageView = image?.pointSized ? view * (size?.scale ?? 1) : view;
  if (els.overlay) {
    if (image) {
      applyCursorCss(els.overlay, image, imageView);
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
  const draw =
    extent > 0 ? Math.max(imageView, MIN_POINTER_CSS_PX / extent) : imageView;
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
    return await gatewayFetch("/api/session", {
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

// The screen this window is on, as the wire carries it: full resolution in CSS
// pixels and density in hundredths. `window.screen` follows the window between
// displays, so reading it fresh is what keeps a re-send honest. Clamped to the
// wire's u16 like a viewport; a screen that big does not exist.
function hostDisplayMsg(): Extract<ClientMsg, { type: "hostDisplay" }> {
  const dim = (px: number) => Math.min(65535, Math.max(1, Math.round(px) || 1));
  return {
    type: "hostDisplay",
    w: dim(screen.width),
    h: dim(screen.height),
    scale: hostScaleHundredths(),
  };
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
  // The target a connect() is waiting on, so the picker can show progress
  // until the server answers with `connected` (or an error).
  const [pendingTarget, setPendingTarget] = useState<string | null>(null);
  // True when the connected target opted into the clipboard bridge, which is
  // what enables the floating menu's Clipboard button.
  const [canClipboard, setCanClipboard] = useState(false);
  // Whether the companion extension has answered. It owns the system clipboard when
  // it has, which is the one thing about this page's behaviour it changes. See
  // companion.ts.
  const companion = useCompanion();
  // Whether this target offers remote audio; this says nothing about activity.
  const [canAudio, setCanAudio] = useState(false);
  // Whether this browser has asked for the sound. Per attachment and never
  // remembered: it starts off on every connect and reconnect, because enabling it
  // has to happen inside a click — that is what makes an AudioContext playable
  // without an autoplay policy's permission.
  const [audioEnabled, setAudioEnabled] = useState(false);
  // Why there is no sound, when there should be. One string, and what is behind it is
  // a decoder that refused or failed — this browser having no WebCodecs at all is not
  // among the possibilities, because such a browser never got past preflight.ts. A
  // refusal is reported rather than worked around: the codec is the gateway's to
  // choose and there is no second representation to fall back to (audioPlayer.ts).
  const [audioError, setAudioError] = useState<string | null>(null);
  // Why this browser is showing nothing for a video target, or null.
  //
  // Kept apart from `connectError` because the session is fine — it is this client
  // that cannot decode what is arriving — and apart from `audioError` because of
  // what it costs. No audio decoder means silence beside a working desktop; no video
  // decoder means no desktop at all, so this needs a surface that stays up while
  // `status` is "connected", which the status overlay does not.
  const [videoError, setVideoError] = useState<string | null>(null);
  // The render dial this session resolved to, from `connected`. Empty in the picker.
  const [renderPlan, setRenderPlan] = useState("");
  // What this session is speaking, from `connected`: the protocol and the target's
  // subtype where it has one. Empty in the picker, and read only by the card — no
  // behaviour hangs off it, because every capability that varies by subtype already
  // arrives as its own flag on the same message. See connectionLabel.ts.
  const [connection, setConnection] = useState("");
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
  // The remembered "sound by default" preference, edited from the picker and
  // from the desktop menu alike (see AUDIO_KEY). Applied to a compatible
  // connection in `handleConnected`, and read there through a ref so the
  // connection effect never re-subscribes when it changes.
  const [audioByDefault, setAudioByDefault] = useState(() =>
    readOnByKey(AUDIO_KEY),
  );
  const audioByDefaultRef = useRef(audioByDefault);
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

  // Mirror the "by default" preference into its ref (the connection effect reads
  // it there) and persist it, whatever set it — the picker's toggle or the
  // desktop menu's live control.
  useEffect(() => {
    audioByDefaultRef.current = audioByDefault;
    try {
      localStorage.setItem(AUDIO_KEY, audioByDefault ? "on" : "off");
    } catch {
      // Storage blocked: the preference still holds for this tab.
    }
  }, [audioByDefault]);

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
  // The painter survives connection-effect reruns on the same canvas, so its
  // attachment epoch must survive them too. Otherwise both the old and new
  // effects start at one and a late old completion can acknowledge the new
  // socket. Starts and teardowns both advance it; no value is reused.
  const paintGenerationRef = useRef(0);
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
  // The same arrangement for the other direction the chrome reaches into a chord:
  // a shortcut the SPA took for itself, with Command in it. Set by the input
  // effect, called by the toolbar. Null while nothing is subscribed, which is also
  // when there is no chord to unwind.
  const localShortcutRef = useRef<(() => void) | null>(null);
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
  // Whether this window drives the remote's size — the target's `resize`, off
  // on a pinch-zoom device whatever the target allows (see CAN_PINCH_ZOOM).
  // There is no client-side mode beside it: the gateway names the policy on
  // `connected` and this client obeys. A ref because the viewport sender lives
  // inside the connection effect and must not re-subscribe when it changes.
  const followWindowRef = useRef(false);

  // Open and close the audio socket from outside the connection effect, which is
  // where the toggle lives. A ref rather than state for the same reason `startRef` is
  // one: the effect owns the sockets, and everything else only asks it to act.
  const audioSocketRef = useRef<{
    open: () => void;
    close: () => void;
  } | null>(null);

  // The engine's latest pointer state, and where the touch gesture layer's
  // virtual pointer sits (null while a hardware mouse is driving). Both are
  // refs: they are pushed straight to the DOM by syncCursor, and no React
  // output depends on them — pointer motion must not re-render.
  const cursorRef = useRef<RemoteCursor | null>(null);
  const touchCursorRef = useRef<Point | null>(null);

  // Re-apply the pointer to the DOM. Called whenever the shape, the virtual
  // pointer position, or the canvas geometry (resize, zoom, pan, dpr) changes.
  //
  // Coalesced to one paint per frame, at the frame boundary: `paintCursor`
  // reads the canvas rect, and its callers — a pinch's applyView above all —
  // have usually just written canvas styles, so an inline read was a forced
  // layout per gesture event. A rAF callback runs after the writes and before
  // the paint, so nothing on screen ever shows the deferral.
  const cursorSyncArmed = useRef(false);
  const syncCursor = useCallback(() => {
    if (cursorSyncArmed.current) {
      return;
    }
    cursorSyncArmed.current = true;
    requestAnimationFrame(() => {
      cursorSyncArmed.current = false;
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
    });
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
    let disposed = false;
    let ws: WebSocket | null = null;
    // Sound has a socket of its own, so that it never queues behind a picture — see
    // src/ws.rs. Opening it *is* the subscription; there is no message for audio.
    let audioWs: WebSocket | null = null;
    // The claim the sockets attach with, kept so audio can be opened and closed at any
    // point in the session rather than only when the session socket is built.
    let session: string | null = null;
    // The parse→decode→paint path — the slot table, the video decoders and the
    // batch draw loop — runs in a worker that owns this canvas's bitmap, so a
    // batch neither draws nor reaches the screen through this thread's input and
    // React work (desktopPainterWorker.ts says what that is and is not worth). This
    // effect only posts to it; the worker itself outlives the effect (see
    // desktopPainter.ts), and `bind` points its callbacks at this run.
    const painter = canvasRef.current
      ? desktopPainterFor(canvasRef.current)
      : null;
    // Resizes in flight to the worker, by sequence number. The state half of a
    // resize — the CSS box, `size`, the cursor — waits for the worker's echo
    // (see `onResized` in desktopPainter.ts for why), and `clearDesktop`
    // abandons whatever is pending so a late echo cannot resurrect a size the
    // attachment it belonged to has already left behind.
    let resizeSeq = 0;
    const pendingResizes = new Map<number, RemoteSize>();
    // The worker outlives socket reconnects, so a completion can return after
    // the socket that posted its frame has died. A generation travels through
    // the worker with each batch; only the live generation may acknowledge on
    // the live socket. Batch sequences alone are insufficient because each new
    // attachment starts them over.
    let paintSocket: WebSocket | null = null;
    painter?.bind({
      onCacheReset: () => sendRef.current({ type: "cacheReset" }),
      onVideoError: setVideoError,
      // A repaint rather than a cache reset: the slot table is not what went
      // wrong, and a repaint is what re-announces every stream's format and arms
      // a keyframe on it (`reset_render` in src/encode.rs). Logged rather than
      // shown — the recovery is a frame away and nothing asked the person for it.
      onVideoNeedsKeyframe: (reason) => {
        console.warn(`video: ${reason}; asking for a repaint`);
        sendRef.current({ type: "refresh" });
      },
      onPainted: (sequence, generation, queuedMs, drawMs) => {
        sendPaintAck(
          paintGenerationRef,
          generation,
          paintSocket,
          ws,
          sequence,
          queuedMs,
          drawMs,
        );
      },
      onResized: (seq) => {
        const applied = pendingResizes.get(seq);
        if (applied) {
          pendingResizes.delete(seq);
          presentResize(applied);
        }
      },
    });
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
      // A resize still waiting on its echo belongs to the attachment this is
      // ending; letting it land later would resurrect that desktop's size
      // under the next one's overlay.
      pendingResizes.clear();
      // Pointer ownership is per-engine: the next target may well composite
      // its own cursor, so drop back to hiding the browser's until it says
      // otherwise. A reattach to the same engine gets the shape replayed.
      cursorRef.current = null;
      touchCursorRef.current = null;
      // One message: the worker zeroes the canvas bitmap it owns and drops the
      // slot table and the decoders with it. The next attachment's server
      // starts with an empty table, so holding any of it would only cost
      // memory. (Nothing could be *drawn* wrongly: a reference always follows
      // the tile that filled its slot on the same socket.)
      painter?.clear();
      // Sound's own socket goes with this one. The gateway would keep the
      // subscription alive across a reattach — it belongs to the claim now — but this
      // browser cannot: rebuilding a decoder needs an AudioContext, and a context
      // built outside a gesture is the thing iOS Safari suspends with no way back. So
      // the next `connected` starts from off, and getting it back is a click.
      releaseAudio();
      closeAudioSocket();
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

    // Viewport reports (dynamic resize), deduped per connection: a resize that
    // settles on the same size sends nothing. One gate: nothing goes out unless
    // the target handed its size to this window — an engine drops the request
    // otherwise, and there is no manual control that could ask for one.
    let lastViewport: { w: number; h: number } | null = null;
    const sendViewport = () => {
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        return;
      }
      if (!followWindowRef.current) {
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
    // The screen this window is on, deduped the same way. Mid-session only its
    // density is acted on — an RDP target that allows resize matches it, and
    // re-sending an unchanged value would be a full RDP reactivation for
    // nothing. The full resolution rides along so the message stays the shape
    // `connect` carries, where the size is what the session opens at.
    let lastHostDisplay: string | null = null;
    // Which display the remote is sharing, as its last `displays` reported it.
    // Only so a switch can be told from the first list of a session.
    let sharedDisplay: number | null = null;
    const sendHostDisplay = () => {
      const msg = hostDisplayMsg();
      // Recorded before either guard below, and whether or not it is sent: the
      // menu shows this number, and a density this screen has that the remote
      // does not is exactly what someone reading it is trying to see.
      setHostScale(msg.scale);
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        return;
      }
      const key = `${msg.w}x${msg.h}@${msg.scale}`;
      if (lastHostDisplay === key) {
        return;
      }
      lastHostDisplay = key;
      sendRef.current(msg);
    };

    const open = (sessionId: string) => {
      session = sessionId;
      const socket = new WebSocket(gatewaySocketUrl("/ws", sessionId));
      const generation = advancePaintGeneration(paintGenerationRef);
      paintSocket = socket;
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
        if (paintSocket === socket) {
          paintSocket = null;
        }
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
      // Binary frames go straight to the paint worker, buffer transferred, in
      // arrival order — postMessage order *is* the draw order, so the promise
      // queue that used to hold draws and draw-ordered control messages in
      // line on this thread lives in the worker now. The control messages
      // whose effects touch what draws touch still keep their place there:
      // `resize` and `videoFormat` post commands behind the frames already
      // sent, and `connected`/`picker` post `clear` the same way (see
      // desktopPainterWorker.ts for why the clear must hold its place too).
      // Their *state* halves run on arrival now rather than behind the
      // backlog, which is fine because every mode they switch to hides the
      // canvas behind an overlay until the worker's queue has caught up.
      // Everything else always ran on arrival: a cursor shape or a clipboard
      // answer gains nothing by queueing behind the worker's draws.
      //
      // `owned` is checked at dispatch — a superseded socket keeps firing
      // `onmessage` until its close lands, and its frames and control messages
      // must not reach the new attachment's worker or state. That check is
      // also what lets the worker run on order alone: a dead socket's frames
      // stop being posted before `clearDesktop` posts the clear that ends
      // their attachment, so nothing can arrive there out of place.
      const owned = () => !disposed && ws === socket;
      const dispatchControl = (text: string) => {
        let msg: ControlMsg;
        try {
          msg = JSON.parse(text) as ControlMsg;
        } catch {
          return;
        }
        handleControlMsg(msg);
      };
      socket.onmessage = (ev) => {
        if (!owned()) {
          return;
        }
        const data = ev.data;
        if (typeof data !== "string") {
          // Sound is on its own socket, so this one carries batches and
          // nothing else; the worker still reads the kind byte rather than
          // assuming it.
          if (data instanceof ArrayBuffer) {
            painter?.draw(data, generation);
          }
          return;
        }
        dispatchControl(data);
      };
    };

    // Sound arrives on its own socket, so nothing it does can be delayed by a repaint
    // and nothing it does can delay one. No promise queue either, unlike the messages
    // above: there is nothing here to keep in order with a tile draw, and the packets
    // are handed straight to WebCodecs, which decodes off-thread and calls back.
    // The only control message this socket carries. Anything else is a gateway that
    // has changed under this build.
    const handleAudioControl = (text: string) => {
      let msg: ControlMsg;
      try {
        msg = JSON.parse(text) as ControlMsg;
      } catch {
        return;
      }
      if (msg.type === "audioFormat") {
        startAudio(msg);
      }
    };

    const handleAudioFrame = (data: ArrayBuffer) => {
      if (binaryFrameKind(data) !== "audio") {
        return;
      }
      const packets = decodeAudioFrame(data);
      if (packets) {
        audioPlayerRef.current?.push(packets);
      }
    };

    const handleAudioMessage = (data: unknown) => {
      if (typeof data === "string") {
        handleAudioControl(data);
      } else if (data instanceof ArrayBuffer) {
        handleAudioFrame(data);
      }
    };

    // Subscribe to the remote's sound by opening its socket, and unsubscribe by
    // closing it. Idempotent in both directions, because every caller is some form of
    // "make it so" rather than "toggle".
    const openAudioSocket = () => {
      closeAudioSocket();
      if (disposed || !session) {
        return;
      }
      const socket = new WebSocket(gatewaySocketUrl("/ws/audio", session));
      socket.binaryType = "arraybuffer";
      audioWs = socket;
      socket.onmessage = (ev) => {
        if (audioWs === socket) {
          handleAudioMessage(ev.data);
        }
      };
      // Nothing to do on close, and deliberately nothing: the session socket owns
      // reconnection, and it will reopen this one through `seedAudioForAttachment`
      // when it comes back. Retrying here as well would race that.
      socket.onclose = () => {
        if (audioWs === socket) {
          audioWs = null;
        }
      };
    };

    const closeAudioSocket = () => {
      const socket = audioWs;
      audioWs = null;
      socket?.close();
    };

    // Build the decoder the format describes, around the context the click made.
    //
    // A *second* format on the same socket is a new desktop — the audio socket
    // outlives a target switch, and the gateway re-announces when it arms the next
    // engine. The decoder built for the previous stream describes something that has
    // ended, so it goes, and its timeline starts over with it.
    const startAudio = (msg: Extract<ControlMsg, { type: "audioFormat" }>) => {
      if (audioPlayerRef.current) {
        releaseAudio();
      }
      // The click's context when there is one, which is the first format after the
      // toggle. Otherwise a fresh one, which is only safe because this socket was
      // opened by a click in the first place: the page has certainly been interacted
      // with by the time a second format arrives, so the context resumes.
      const context = audioContextRef.current ?? createAudioContext();
      audioContextRef.current = context;
      try {
        const player = createAudioPlayer(
          {
            codec: msg.codec,
            sampleRate: msg.sampleRate,
            channels: msg.channels,
            packetFrames: msg.packetFrames,
            head: decodeAudioHead(msg.head),
          },
          context,
          {
            onError: (reason) => {
              releaseAudio();
              setAudioEnabled(false);
              setAudioError(reason);
              // And stop the packets, which would otherwise keep arriving for a
              // decoder that has gone. Closing the socket is how that is said.
              closeAudioSocket();
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
        closeAudioSocket();
      }
    };

    // The state half of a resize, run when the worker's echo says the bitmap
    // it describes is real. Running it on the control message's arrival
    // instead would read as a glimpse of the previous desktop: the overlay
    // hides the canvas only while `size` is null, and the worker could still
    // be painting the old attachment's backlog onto the old bitmap.
    const presentResize = (s: RemoteSize) => {
      applyCanvasCss(
        canvasRef.current,
        s,
        viewRef.current,
        bottomInsetRef.current,
      );
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

    const handleResize = (msg: Extract<ControlMsg, { type: "resize" }>) => {
      const s = { w: msg.w, h: msg.h, scale: msg.scale > 0 ? msg.scale : 1 };
      if (!painter) {
        // No canvas, so nothing queues either; the state may as well be true.
        presentResize(s);
        return;
      }
      // The bitmap belongs to the worker; this command queues behind the
      // frames already posted, which is the place the old draw-ordered queue
      // gave a resize — the previous desktop finishes painting before its
      // canvas is replaced and filled black.
      const seq = ++resizeSeq;
      pendingResizes.set(seq, s);
      painter.resize(desktopCanvasGeometry(s, s.scale).bitmap, seq);
    };

    // The remote's display list, and the one follow-up a change of display needs:
    // this screen's density again. Standard macOS Screen Sharing sends its physical
    // displays here; High Performance mode reports its single virtual display.
    const handleDisplays = (msg: Extract<ControlMsg, { type: "displays" }>) => {
      setDisplays(msg.displays);
      setActiveDisplayId(msg.active);
      const switched = sharedDisplay !== null && sharedDisplay !== msg.active;
      sharedDisplay = msg.active;
      if (switched) {
        lastHostDisplay = null;
        sendHostDisplay();
      }
    };

    // Audio belongs to one attachment: whatever was playing was on a socket that
    // is gone, so a subscription has to be asked for again, and that ask needs a
    // click's gesture for the AudioContext to be allowed to play. So the only way
    // sound comes up already on is when the user wants it by default *and* this
    // connect carried a gesture — `connect` primed a context inside the picker
    // click, which is the only place `audioContextRef` is set — *and* the target
    // actually carries audio. Anything else (a reattach with no gesture, a target
    // with no sound, the default off) starts silent.
    const seedAudioForAttachment = (hasAudio: boolean) => {
      setAudioError(null);
      if (audioByDefaultRef.current && hasAudio && audioContextRef.current) {
        setAudioEnabled(true);
        openAudioSocket();
      } else {
        releaseAudio();
        closeAudioSocket();
        setAudioEnabled(false);
      }
    };

    const handleConnected = (
      msg: Extract<ControlMsg, { type: "connected" }>,
    ) => {
      // A target session started (picker connect, reattach, or takeover of a
      // live desktop): switch to the desktop.
      setConnectError(null);
      setPendingTarget(null);
      setMode("desktop");
      setCanClipboard(msg.clipboard);
      setCanAudio(msg.audio);
      seedAudioForAttachment(msg.audio);
      // What this session is, for the card. Nothing is checked here: whether this
      // browser can decode what a streaming target sends is answered by `configure`
      // refusing it, once, with the configuration in hand.
      setRenderPlan(msg.render);
      setConnection(connectionLabel(msg.protocol, msg.subtype));
      lastViewport = null;
      if (CAN_PINCH_ZOOM) {
        // Mobile has one rule and it does not vary by protocol: ask once, here,
        // and never let this window's shape reach the remote again — the window
        // it would resize to is the one this client deliberately does not ask
        // the remote to be.
        //
        // The one-shot is still gated on the target's `resize`, because there is
        // nothing to say otherwise: an engine drops the request without it.
        followWindowRef.current = false;
        mobileSizePending = msg.resize;
      } else {
        // The gateway's one switch: `resize` means this window drives the
        // remote's size, and there is nothing to toggle beside it. Report at
        // once rather than waiting for the next window resize — the remote
        // opened at this screen's full resolution, and "follows this window"
        // that starts by not matching it would read as broken. The dedupe makes
        // it free when it already matches.
        followWindowRef.current = msg.resize;
        sendViewport();
      }
      // And this window's screen, so a resizable remote uses the browser's
      // backing/logical ratio rather than whatever density it last used.
      lastHostDisplay = null;
      sharedDisplay = null;
      sendHostDisplay();
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
      // Inside `remotex.app` the app owns the pasteboard and writes it with
      // AppKit. Not a preference: `navigator.clipboard.writeText` in a web view
      // has no user gesture behind it, so it is refused, and the remote's copy
      // would silently never arrive on the Mac.
      if (NATIVE_HOST) {
        postToHost({ type: "clipboardFromRemote", text });
        return;
      }
      // The companion extension owns the system clipboard for the same reason the
      // app does — its offscreen document can write without a gesture and without
      // focus, and a page can do neither.
      //
      // *Instead of* the line below, never as well as: two writers race, and the
      // extension would then read the page's own write back off the clipboard as a
      // foreign copy and push it to the remote as though the user had copied it here.
      if (postToCompanion({ type: "clipboardFromRemote", text })) {
        return;
      }
      void navigator.clipboard.writeText(text).catch(() => {});
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
                  pointSized: msg.pointSized,
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
        case "connected":
          handleConnected(msg);
          break;
        case "audioFormat":
          startAudio(msg);
          break;
        case "videoFormat":
          // Straight to the painter, which owns the decoders — queued in the
          // worker behind the frames already posted, because a changed decode
          // string drops a live decoder that queued units still need. A browser
          // that cannot decode what it names finds out from the decoder's own
          // error, which arrives at `onVideoError` naming the configuration.
          painter?.setVideoFormat(msg.stream, {
            decode: msg.decode,
          });
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
            // A copy on the remote is immediately pastable here. Best effort by
            // design: `writeText` rejects when the tab is unfocused, which is
            // exactly when a remote copy is most likely to arrive. Never mirror an
            // empty push, which would wipe the local clipboard for a non-text
            // remote copy — a refused oversized copy arrives as one of those, and
            // the panel is where its size is reported.
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
          // No engine to resize: the next target states its own policy.
          followWindowRef.current = false;
          setCanClipboard(false);
          // No engine, so no queue to subscribe to: the row goes away rather than
          // offering a control that would be answered with a warning in the log.
          setCanAudio(false);
          releaseAudio();
          closeAudioSocket();
          setAudioEnabled(false);
          setAudioError(null);
          // The stream itself goes with `clearDesktop` below; what has to be said
          // here is that the complaint goes too. Whatever this browser could not
          // decode is no longer on the screen, and the next target may not send
          // video at all.
          setVideoError(null);
          setRenderPlan("");
          setConnection("");
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
    audioSocketRef.current = { open: openAudioSocket, close: closeAudioSocket };
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
        // A window that just resized may have been dragged to another display;
        // `window.screen` follows it and the dedupe makes an unchanged one free.
        sendHostDisplay();
      }, 250);
    };
    window.addEventListener("resize", onViewportChange);

    // A devicePixelRatio change affects what a resizable remote is asked to
    // render, not the canvas's layout: the bitmap and CSS box describe the
    // guest's pixels and points, while the browser rasterizes them for the host.
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
      sendHostDisplay();
      watchDpr();
    }
    watchDpr();

    return () => {
      disposed = true;
      advancePaintGeneration(paintGenerationRef);
      paintSocket = null;
      startRef.current = null;
      audioSocketRef.current = null;
      audioWs?.close();
      // The socket is going away, so nothing will answer a pending fetch.
      settleClipboardWaiters(null);
      clearTimeout(retryTimer);
      window.removeEventListener("resize", onViewportChange);
      dprQuery?.removeEventListener("change", onDprChange);
      clearTimeout(resizeTimer);
      ws?.close();
      // The worker outlives this effect — its canvas element can only be
      // transferred once, and StrictMode reruns the effect on the same element
      // (see desktopPainter.ts) — but what it holds must not: under
      // `render_type = "video"` that includes a `VideoDecoder`, hardware
      // rather than just memory. `clearDesktop` frees it on every path
      // *through* the session; this is the one that leaves.
      painter?.clear();
      painter?.unbind();
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
  // server answers `connected` (→ desktop) or `error` (shown on the picker).
  const connect = useCallback(
    (target: string) => {
      setConnectError(null);
      setPendingTarget(target);
      // If the user wants sound by default, spend this click's gesture on an
      // AudioContext now — the only moment one is playable (see setAudio). The
      // `connected` that decides whether the target actually carries audio arrives
      // a round trip later, long past any gesture, so it cannot make one then:
      // `handleConnected` either adopts this primed context or, when the target has
      // no audio or the default is off, releases it. Primed without asking whether
      // this browser can decode: that depends on the target's codec, which the
      // `audioFormat` a round trip later is the first thing to say — and a context
      // that turns out unusable is released there at no cost.
      if (audioByDefaultRef.current) {
        releaseAudio();
        audioContextRef.current = createAudioContext();
      }
      // The connect names this window's screen, so a target with no pinned
      // config size opens at its full resolution — before any message this
      // client could send after the fact.
      const { w, h, scale } = hostDisplayMsg();
      sendRef.current({ type: "connect", target, display: { w, h, scale } });
    },
    [releaseAudio],
  );

  // Switch target: tear the current session down and return to the picker. The
  // server answers `picker`, which flips `mode` back.
  const switchTarget = useCallback(() => {
    sendRef.current({ type: "disconnect" });
  }, []);

  // Share a different one of the remote's displays (the Display panel). Fire
  // and forget, and deliberately not optimistic: the answer is the remote's
  // next `displays`, which is what moves the checkmark. A no-op while the
  // socket is down.
  const selectDisplay = useCallback((id: number) => {
    sendRef.current({ type: "selectDisplay", id });
  }, []);

  // Re-announce the size and repaint everything, for a canvas that has gone
  // wrong. Only the native shell offers it — a browser has reload, which does
  // this and more.
  const refresh = useCallback(() => {
    sendRef.current({ type: "refresh" });
  }, []);

  // Start or stop the remote's sound (the floating menu's Audio button).
  //
  // **Must be called from a click**, and the AudioContext is why: a context created
  // inside a user gesture may play, and one created outside it is suspended on iOS
  // Safari with no way back. The decoder cannot be built here — `audioFormat` has not
  // arrived yet — so the context is what the gesture is spent on, and `startAudio`
  // wraps a decoder around it a round trip later.
  //
  // Opening the socket is the whole of the request; there is no message for audio any
  // more, and there is nothing to acknowledge one. The honest reading of "enabled" is
  // that this browser asked and is holding a context open for the answer. A gateway
  // with nothing to send simply sends nothing on a socket that stays open.
  const setAudio = useCallback(
    (enabled: boolean) => {
      // The live control also writes the remembered default, so a choice made
      // mid-session is the one the next connect starts from — the same single value
      // the picker's toggle edits. Recorded as the intent whether or not this
      // browser can decode: the picker's checkbox then honestly reflects what was
      // asked for, and a capable browser later in the same profile obeys it.
      setAudioByDefault(enabled);
      setAudioError(null);
      setAudioEnabled(enabled);
      releaseAudio();
      if (enabled) {
        // Asked for without checking whether this browser can decode this target's
        // codec, because that is not answerable yet: only the `audioFormat` that
        // comes back names one. `startAudio` is where the question can be asked, and
        // the decoder's own `onError` reports the answer a round trip later.
        audioContextRef.current = createAudioContext();
        audioSocketRef.current?.open();
      } else {
        audioSocketRef.current?.close();
      }
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
  // (from an engine-side buffer for VNC and RDP), so this behaves the same on
  // both.
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
  // ClientCutText nor Apple's pasteboard write is acknowledged, so there is
  // nothing to await.
  const sendClipboard = useCallback((text: string) => {
    // Recorded even for a manual Send from the panel: some VNC servers echo a
    // cut straight back as ServerCutText, and without this the round trip
    // would overwrite the local clipboard with what we just sent.
    lastToRemoteRef.current = text;
    sendRef.current({ type: "clipboard", text });
  }, []);

  // The host's clipboard changed and something outside this page noticed. Only
  // the native shell has such a thing — it polls `NSPasteboard.changeCount` —
  // and it lands here rather than in `sendClipboard` so it passes the same echo
  // guards the browser's own focus push does: a value that came *from* the remote
  // a moment ago must not go straight back to it.
  const pushLocalClipboard = useCallback(
    (text: string) => {
      // The shell polls the pasteboard and stops when it thinks the session cannot
      // take one, but what it thinks is one IPC hop behind what this page knows —
      // and the session it *would* reach after a target switch is a different
      // machine. So the page decides too, from the same two values its own focus
      // push uses.
      if (mode !== "desktop" || !canClipboard) {
        return;
      }
      if (
        text === "" ||
        overClipboardLimit(text) ||
        text === lastFromRemoteRef.current ||
        text === lastToRemoteRef.current
      ) {
        return;
      }
      lastToRemoteRef.current = text;
      sendRef.current({ type: "clipboard", text });
    },
    [mode, canClipboard],
  );

  // Best-effort clipboard push on focus, when reads are permitted. Oversized
  // values are skipped locally; the explicit panel reports the limit.
  //
  // Not in the native shell, where the app polls `NSPasteboard.changeCount` and
  // pushes what it finds: reading the pasteboard from a page there would ask macOS
  // for permission a second time, on behalf of a "browser" the user cannot see.
  //
  // Not under the companion extension either, and for the same reason: its offscreen
  // document polls the system clipboard whether this window has focus or not, so a
  // second reader here would push the same text twice and put the browser's clipboard
  // prompt on screen on top of it.
  //
  // `!== "absent"` rather than `=== "connected"`. The companion answers
  // asynchronously, and standing down while the answer is still unknown is what stops
  // a duplicate push in the first second of a session that turns out to have one.
  // When it settles to `absent` this effect re-runs and the trailing call below covers
  // the delay.
  useEffect(() => {
    if (
      NATIVE_HOST ||
      companion !== "absent" ||
      mode !== "desktop" ||
      !canClipboard
    ) {
      return;
    }
    const pushBrowserClipboardOnFocus = () => {
      if (document.hidden || !document.hasFocus()) {
        return;
      }
      void (async () => {
        let text: string;
        try {
          text = await navigator.clipboard.readText();
        } catch {
          return; // no permission, or the user declined
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
    window.addEventListener("focus", pushBrowserClipboardOnFocus);
    document.addEventListener("visibilitychange", pushBrowserClipboardOnFocus);
    // Also once now: the tab may already be focused when the session starts.
    pushBrowserClipboardOnFocus();
    return () => {
      window.removeEventListener("focus", pushBrowserClipboardOnFocus);
      document.removeEventListener(
        "visibilitychange",
        pushBrowserClipboardOnFocus,
      );
    };
  }, [mode, canClipboard, companion]);

  // A live session is a thing to lose, and ⌘W or Ctrl+W closes a tab before this page
  // sees the key — except under a keyboard lock, where they arrive as ordinary
  // keydowns and go to the remote instead. So the mitigation is the browser's own
  // leave-site dialog, which needs sticky activation: a desktop the user has clicked
  // on always has it.
  //
  // `mode`, not `status`: a reconnecting session is still one worth not closing. Not
  // in `remotex.app`, where closing the window is the app's own quit path and this
  // would put a web dialog in front of it.
  useEffect(() => {
    if (NATIVE_HOST || mode !== "desktop") {
      return;
    }
    const guard = (event: BeforeUnloadEvent) => {
      event.preventDefault();
    };
    window.addEventListener("beforeunload", guard);
    return () => {
      window.removeEventListener("beforeunload", guard);
    };
  }, [mode]);

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

  // The toolbar took a chord that had Command in it. Stable, so the handler that
  // reports it does not resubscribe, and a no-op unless a Command is pending.
  const onLocalShortcut = useCallback(() => {
    localShortcutRef.current?.();
  }, []);

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
    // `remotex.app` is given every Command chord for the whole session, so it gets
    // the fuller table outright. A browser is given them only while a keyboard lock
    // is held, and that comes and goes under a running session — a held Esc ends one
    // without asking — so the table follows the lock rather than being chosen once.
    const macKeys = new MacKeyboardTranslator(
      NATIVE_HOST || keyboardLockHeld(),
    );
    const stopWatchingLock = onKeyboardLockChange((locked) => {
      macKeys.setCapturesEveryChord(NATIVE_HOST || locked);
    });

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

    // Scroll and window resize move the canvas without going through
    // `applyCanvasCss`, so they invalidate the pointer rect cache themselves —
    // scroll on capture, because the scrolling element is whichever ancestor
    // overflowed, and scroll events do not bubble.
    const invalidatePointerRect = () => pointerRectCache.invalidate();

    const toRemote = (e: MouseEvent) => {
      // Map through the canvas rect (not the overlay): it reflects the
      // displayed framebuffer under the current touch zoom/pan, and on
      // desktop it coincides with the overlay anyway.
      const rect = pointerRectCache.read(canvasRef.current ?? el);
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
      send({
        type: "mouseButton",
        button,
        pressed: true,
        clicks: clickCount(e.detail),
      });
    };
    // Release on window so a press that ends outside the overlay still reports
    // the button up. Only buttons we saw pressed on the surface are released.
    const onMouseUp = (e: MouseEvent) => {
      const button = mouseButtonFromEvent(e.button);
      if (!button || !pressedButtons.delete(button)) {
        return;
      }
      send({
        type: "mouseButton",
        button,
        pressed: false,
        clicks: clickCount(e.detail),
      });
    };
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      send({
        type: "wheel",
        dx: e.deltaX,
        dy: e.deltaY,
        unit: wheelUnitFromEvent(e.deltaMode),
      });
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
        // A sweep, not a gesture: one click is what an unclick is worth.
        send({ type: "mouseButton", button, pressed: false, clicks: 1 });
      }
      pressedButtons.clear();
      gestures?.release();
    };
    // Every key goes through the Command translator, which is a pass-through
    // unless this is a Mac host driving a non-Mac guest. `pressedKeys` follows what
    // it emits rather than what arrived, so releaseAll can undo a chord the guest
    // was told about in different codes than the user typed.
    //
    // Taken apart from the DOM event so the translation and the held-key
    // bookkeeping have one home. Inside `remotex.app` the chords a browser never
    // sees — ⌘W, ⌘Q, ⌘T — arrive here as ordinary `keydown` events, because the
    // shell drops its menu accelerators while the desktop has focus rather than
    // injecting keys down a side channel. See nativeHost.ts.
    const emitKey = (
      code: string,
      pressed: boolean,
      caps: boolean,
      meta: boolean,
    ) => {
      const translated = macKeys.translate(
        { code, pressed, caps, meta },
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
    const sendTranslated = (e: KeyboardEvent, pressed: boolean) => {
      e.preventDefault();
      emitKey(e.code, pressed, e.getModifierState("CapsLock"), e.metaKey);
    };
    const onKeyDown = (e: KeyboardEvent) => sendTranslated(e, true);
    const onKeyUp = (e: KeyboardEvent) => sendTranslated(e, false);
    const onBlur = () => releaseAll();
    releaseKeysRef.current = releaseKeys;
    localShortcutRef.current = () => macKeys.noteCommandUsedLocally();

    el.addEventListener("mousemove", onMouseMove);
    el.addEventListener("mousedown", onMouseDown);
    window.addEventListener("mouseup", onMouseUp);
    window.addEventListener("scroll", invalidatePointerRect, {
      capture: true,
      passive: true,
    });
    window.addEventListener("resize", invalidatePointerRect);
    el.addEventListener("wheel", onWheel, { passive: false });
    el.addEventListener("contextmenu", onContextMenu);
    // Keyboard is scoped to the focused overlay (not window) so the remote
    // surface only grabs keys when the user is interacting with it.
    el.addEventListener("keydown", onKeyDown);
    el.addEventListener("keyup", onKeyUp);
    el.addEventListener("blur", onBlur);

    return () => {
      gestures?.detach();
      stopWatchingLock();
      releaseKeysRef.current = null;
      localShortcutRef.current = null;
      el.removeEventListener("mousemove", onMouseMove);
      el.removeEventListener("mousedown", onMouseDown);
      window.removeEventListener("mouseup", onMouseUp);
      window.removeEventListener("scroll", invalidatePointerRect, {
        capture: true,
      });
      window.removeEventListener("resize", invalidatePointerRect);
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
    hostScale,
    renderPlan,
    connection,
    canClipboard,
    canAudio,
    audioEnabled,
    audioError,
    videoError,
    // The remembered "by default" preference and its setter, for the picker's
    // toggle.
    audioByDefault,
    setAudioByDefault,
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
    onLocalShortcut,
    takeOver,
    retry,
    connect,
    switchTarget,
    selectDisplay,
    refresh,
    setAudio,
    sendKeyCombo,
    requestClipboard,
    sendClipboard,
    pushLocalClipboard,
    setBottomInset,
  };
}
