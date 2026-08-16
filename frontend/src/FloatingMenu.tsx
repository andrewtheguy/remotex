import {
  type PointerEvent as ReactPointerEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import { appWindow, onAppWindowChange } from "./appWindow.ts";
import { ClipboardPanel } from "./ClipboardPanel.tsx";
import DisplayPanel from "./DisplayPanel.tsx";
import { desktopViewportSize, sizeWindowToDesktop } from "./desktopWindow.ts";
import {
  type AudioRow,
  type AudioStreamInfo,
  audioLabel,
  videoLabel,
} from "./mediaLabel.ts";
import type {
  ClipboardSnapshot,
  DisplayInfo,
  RemoteClipboard,
} from "./protocol.ts";
import { SoftKeyboardPanel } from "./SoftKeyboardPanel.tsx";
import {
  CAN_PINCH_ZOOM,
  densityLabel,
  type RemoteSize,
} from "./useRemoteDesktop.ts";

// The floating chrome — a draggable ☰ button that toggles a toolbar drawer. The
// drawer carries this project's controls, a Switch target button that returns to
// the post-login picker, and Log out, which ends the web login. Three of its
// buttons open a panel instead of acting: Soft keyboard — which is where every
// key, modifier and browser-swallowed combo now lives — Clipboard, only for
// targets that opted into it (see ClipboardPanel), and Display, only for a remote
// that offers more than one (see DisplayPanel).
const FAB_SIZE = 40;
const FAB_MARGIN = 12;
// Pointer travel (px) before a press becomes a drag rather than a click.
const DRAG_THRESHOLD = 6;
const TOOLBAR_WIDTH = 240;
const TOOLBAR_GAP = 10;
const TOOLBAR_MIN_HEIGHT = 120;

// The chrome shortcut, spelled the way the Help card and the button's tooltip show
// it. One source for all three so the card, the tooltip and the handler cannot
// drift — the handler matches exactly what this returns.
//
// The middle modifier is the host's: Command on a Mac, where Option is a
// character-composing key and Ctrl+Alt chords are Windows vocabulary, and Alt
// everywhere else. Whichever it is, the *other* one has to be absent, so all four
// held — a Mac user's own Cmd+Ctrl+Alt+Shift+; hyper chord — stays theirs.
function hideChromeShortcut(isMacHost: boolean): string {
  return isMacHost ? "Ctrl + Cmd + Shift + ;" : "Ctrl + Alt + Shift + ;";
}

// The reference density both ends of this agree on: one CSS pixel per dot, and
// also what RDP calls 100%. So a 2x screen is 192 dpi whichever end names it.
const CSS_DPI = 96;

// A density in the unit a host's own display settings talk in, since that is where
// someone checks whether a remote actually applied one.
function dpiLabel(hundredths: number): string {
  return `${Math.round((hundredths / 100) * CSS_DPI)} dpi`;
}

// What the Mac key override does, which is more than fits on the button that
// toggles it — hence "Mac key override: on" there and the detail here. Shown only
// on a Mac host, the only place that button exists. Mirrors macKeys.ts, including
// the six chords a browser keeps whatever this is set to.
const MAC_KEY_HELP: readonly { situation: string; effect: string }[] = [
  {
    situation: "While on",
    effect: "⌘A ⌘C ⌘F ⌘P ⌘S ⌘V ⌘X ⌘Z arrive as Ctrl chords",
  },
  { situation: "While off", effect: "Every key arrives as pressed" },
  { situation: "⌘ on its own", effect: "Arrives as the Windows key" },
  { situation: "Kept by this browser", effect: "⌘W ⌘T ⌘N ⌘L ⌘O ⌘R" },
  { situation: "A Mac remote", effect: "n/a — ⌘ is sent as ⌘" },
];

// The touch gesture cheat-sheet, mirroring touchGestures.ts.
const GESTURE_HELP: readonly { gesture: string; action: string }[] = [
  { gesture: "Tap", action: "Left-click" },
  { gesture: "Double-tap and hold", action: "Grab, then drag" },
  { gesture: "One-finger drag", action: "Move cursor + pan" },
  { gesture: "Two-finger tap", action: "Right-click" },
  { gesture: "Two-finger pinch", action: "Zoom" },
  { gesture: "Three-finger swipe", action: "Scroll" },
];

interface Position {
  x: number;
  y: number;
}

interface DragState {
  pointerId: number;
  startX: number;
  startY: number;
  originX: number;
  originY: number;
  dragged: boolean;
}

// visualViewport tracks the *visible* area (mobile URL bar, on-screen keyboard,
// pinch-zoom), with a window fallback for browsers that lack it.
interface Viewport {
  width: number;
  height: number;
  offsetX: number;
  offsetY: number;
}

function readViewport(): Viewport {
  const vp = window.visualViewport;
  return {
    width: vp ? vp.width : window.innerWidth,
    height: vp ? vp.height : window.innerHeight,
    offsetX: vp ? vp.offsetLeft : 0,
    offsetY: vp ? vp.offsetTop : 0,
  };
}

// Which docked panel is open, if any.
//
// One state rather than a boolean each: both dock to the bottom edge and report
// the same canvas inset, so a second one open would sit on the first. Two
// booleans made that a rule every call site had to remember — open this one,
// clear the other — and this makes it impossible to express.
type Panel = "clipboard" | "keyboard" | "display";

/// The window kind, subscribed to rather than read once.
///
/// *Install page as app…* reparents this very document into the new window, so every
/// line below that depends on the answer has to be able to change its mind — that is
/// the whole of what `onAppWindowChange` exists for. See appWindow.ts.
function useAppWindow(): boolean {
  return useSyncExternalStore(onAppWindowChange, appWindow, () => false);
}

/// The recommendation, shown only to the window that is not taking it.
///
/// A tab is the one configuration where the browser keeps chords back from the remote,
/// and the fix is a menu item rather than anything this client can do — so saying so is
/// the whole of what it can offer.
function AppWindowHelpRow() {
  // Subscribed, not read: the row is telling the user to install this page as an app,
  // and doing so must make the row itself go away without a reload.
  if (useAppWindow()) {
    return null;
  }
  return (
    <div className="help-item">
      <dt>Give this window every shortcut</dt>
      <dd>
        Chrome menu → Install page as app. ⌘W, Ctrl+W and ⌘T then reach the
        remote
      </dd>
    </div>
  );
}

function usePanel() {
  const [panel, setPanel] = useState<Panel | null>(null);
  const closePanel = useCallback(() => setPanel(null), []);
  const togglePanel = useCallback(
    (next: Panel) => setPanel((prev) => (prev === next ? null : next)),
    [],
  );
  return { panel, setPanel, closePanel, togglePanel };
}

// Whichever docked panel is open, or nothing. Rendering both from one place is
// what makes the shared inset channel safe: exactly one of them is mounted, so
// exactly one is reporting a height.
function DockedPanel({
  panel,
  onClose,
  onDockedHeightChange,
  sendKeyCombo,
  remoteClipboard,
  onSendClipboard,
  displays,
  activeDisplayId,
  onSelectDisplay,
}: {
  panel: Panel | null;
  onClose: () => void;
  onDockedHeightChange: (px: number) => void;
  sendKeyCombo: (codes: string[]) => void;
  remoteClipboard: RemoteClipboard | null;
  onSendClipboard: (text: string) => void;
  displays: DisplayInfo[];
  activeDisplayId: number | null;
  onSelectDisplay: (id: number) => void;
}) {
  switch (panel) {
    case "keyboard":
      return (
        <SoftKeyboardPanel
          sendKeyCombo={sendKeyCombo}
          onClose={onClose}
          onDockedHeightChange={onDockedHeightChange}
        />
      );
    case "clipboard":
      return (
        <ClipboardPanel
          onSend={onSendClipboard}
          remoteClipboard={remoteClipboard}
          onClose={onClose}
          onDockedHeightChange={onDockedHeightChange}
        />
      );
    case "display":
      return (
        <DisplayPanel
          displays={displays}
          activeId={activeDisplayId}
          onSelect={onSelectDisplay}
          onClose={onClose}
          onDockedHeightChange={onDockedHeightChange}
        />
      );
    default:
      return null;
  }
}

// The drawer's Display row, naming the screen currently being shared.
//
// Absent rather than disabled when there is no choice to make, which is the
// opposite of the Clipboard row beside it — and the difference is real. A
// target without the clipboard bridge has a feature that was switched off, and
// a greyed button saying so is the answer. A target with one screen has no
// display feature at all, and a permanently greyed "Display" would be an
// explanation of nothing.
function DisplaySection({
  displays,
  activeDisplayId,
  open,
  onToggle,
}: {
  displays: DisplayInfo[];
  activeDisplayId: number | null;
  open: boolean;
  onToggle: () => void;
}) {
  if (displays.length <= 1) {
    return null;
  }
  // Undefined for the moment between a switch and the remote's answer, and for
  // a screen unplugged out from under the session.
  const active = displays.find((display) => display.id === activeDisplayId);
  return (
    <div className="toolbar-section">
      <span className="toolbar-label">Display</span>
      {/* Unlike Clipboard, this opens straight away: the list is pushed, so
          there is nothing to fetch and nothing to wait for. */}
      <button
        type="button"
        className="toolbar-btn"
        onClick={onToggle}
        aria-pressed={open}
        title="Choose which of the remote's displays to view"
      >
        {open ? "Hide displays" : (active?.label ?? "Display")}
      </button>
    </div>
  );
}

// Resize the browser frame, not the desktop: the resulting content viewport is
// exactly the remote's logical size, leaving applyCanvasCss at its invariant 100%.
// App windows only — a tab's browser frame is not the page's to resize — and not on
// touch clients, whose window cannot be resized and whose presentation is the one
// deliberate fit-to-width exception.
function WindowSection({
  size,
  onSize,
}: {
  size: RemoteSize | null;
  onSize: () => void;
}) {
  const inAppWindow = useAppWindow();
  if (!inAppWindow || CAN_PINCH_ZOOM) {
    return null;
  }
  const viewport = size ? desktopViewportSize(size, size.scale) : null;
  return (
    <div className="toolbar-section">
      <span className="toolbar-label">Window</span>
      <button
        type="button"
        className="toolbar-btn"
        onClick={onSize}
        disabled={!viewport}
        title="Resize this app window so its content area exactly matches the remote desktop"
      >
        {viewport
          ? `Size to ${viewport.w}×${viewport.h}`
          : "Waiting for desktop size"}
      </button>
    </div>
  );
}

// What the remote is drawing against what this browser is, at the top of the Help
// card so the two can be read off one another.
//
// It exists because a density that did not take is otherwise invisible. Both
// engines that match a client's density report the result only as a `resize`, and
// a request the remote quietly dropped produces no message at all: the desktop
// simply looks soft, or half the size it was asked for, with nothing saying which
// end disagreed. Two densities that ought to agree and don't is the whole
// diagnostic, which is why this reports both and not just the resolution.
//
// Shown for every target, not only the ones with a display to switch between: on
// RDP and on VNC the Display section is absent and these numbers appear nowhere
// else.
function ScreenHelp({
  size,
  hostScale,
  connection,
  renderPlan,
  audio,
  videoStreams,
}: {
  size: RemoteSize | null;
  hostScale: number;
  connection: string;
  renderPlan: string;
  audio: AudioRow;
  videoStreams: readonly string[];
}) {
  const video = videoLabel(videoStreams);
  return (
    <>
      <h3>This session</h3>
      <dl className="help-list">
        <div className="help-item">
          <dt>Remote desktop</dt>
          {/* Null before the first `resize`, which is the "waiting for the remote
              desktop" state: a placeholder reading 0×0 would be a worse answer
              than saying so. */}
          <dd>
            {size
              ? `${size.w}×${size.h} at ${densityLabel(size.scale * 100)} (${dpiLabel(size.scale * 100)})`
              : "Waiting for the remote desktop"}
          </dd>
        </div>
        <div className="help-item">
          <dt>This browser</dt>
          <dd>
            {densityLabel(hostScale)} ({dpiLabel(hostScale)})
          </dd>
        </div>
        <div className="help-item">
          <dt>Connection</dt>
          {/* Which of the three `vnc` targets this is, where it is one of them.
              Nothing else on screen distinguishes a plain VNC server from a Mac in
              either Screen Sharing mode, and what a person notices — a display
              list, whether the desktop follows the window, a path with no
              specification behind it — follows from exactly that. Empty only
              before `connected`. */}
          <dd>{connection || "Waiting for the target"}</dd>
        </div>
        <div className="help-item">
          <dt>Render</dt>
          {/* The dial the gateway resolved, which is the one property of a session that
              decides how the picture looks and costs and that nothing else reveals: it
              lives in the operator's config file, which whoever is looking at the screen
              usually does not have. Empty only before `connected`. */}
          <dd>{renderPlan || "Waiting for the target"}</dd>
        </div>
        <div className="help-item">
          <dt>Audio</dt>
          {/* The row above describes the picture and says nothing about the sound,
              which is a separate per-target choice made in the same config file:
              which of the two audio paths this target uses, at what rate, and — the
              one state that is a fault rather than a setting — why a decoder
              stopped. See mediaLabel.ts. */}
          <dd>{audioLabel(audio)}</dd>
        </div>
        {/* Absent for every session with no video in it, which the Render row has
            already said: a "none" here would be repeating it. */}
        {video && (
          <div className="help-item">
            <dt>Video decoder</dt>
            {/* The exact WebCodecs configuration each decoder was built with. It is
                what a `VideoDecoder` complaint names, and until this row it was
                readable only in the console — on a session that is *working*, not
                one that failed, which is when the question is usually asked. */}
            <dd>{video}</dd>
          </div>
        )}
      </dl>
    </>
  );
}

// The direct audio toggle is also the user gesture required to create a
// playable AudioContext. Targets without audio omit the row.
function AudioSection({
  available,
  enabled,
  error,
  onChange,
}: {
  available: boolean;
  enabled: boolean;
  error: string | null;
  onChange: (enabled: boolean) => void;
}) {
  if (!available) {
    return null;
  }
  return (
    <div className="toolbar-section">
      <span className="toolbar-label">Audio</span>
      <button
        type="button"
        className="toolbar-btn"
        onClick={() => onChange(!enabled)}
        aria-pressed={enabled}
        title="Play the remote's sound in this browser"
      >
        {enabled ? "Disable audio" : "Enable audio"}
      </button>
      {/* Quiet remotes have no distinct client-visible state. */}
      {error && <p className="audio-note">{error}</p>}
    </div>
  );
}

// The camera toggle is also the user gesture `getUserMedia`'s permission prompt
// requires. Targets without a camera omit the row — the same rule as Audio's.
// Unlike Audio there is no remembered default anywhere: this button is the one
// and only way the camera turns on, per session, every session.
function CameraSection({
  available,
  enabled,
  error,
  streaming,
  onChange,
}: {
  available: boolean;
  enabled: boolean;
  error: string | null;
  // Whether the remote is consuming right now — an application over there has
  // the camera open. Worded rather than implied, because "enabled and idle" is
  // the normal state until one does.
  streaming: boolean;
  onChange: (enabled: boolean) => void;
}) {
  if (!available) {
    return null;
  }
  return (
    <div className="toolbar-section">
      <span className="toolbar-label">Camera</span>
      <button
        type="button"
        className="toolbar-btn"
        onClick={() => onChange(!enabled)}
        aria-pressed={enabled}
        title="Offer this browser's camera to the remote"
      >
        {enabled ? "Disable camera" : "Enable camera"}
      </button>
      {enabled && !error && (
        <p className="audio-note">
          {streaming
            ? "The remote is using the camera"
            : "Waiting for the remote to open the camera"}
        </p>
      )}
      {error && <p className="audio-note">{error}</p>}
    </div>
  );
}

// The microphone toggle: the camera's twin, and the same rules — a target
// without `microphone = true` omits the row, the enable is the `getUserMedia`
// gesture, and nothing is remembered between sessions.
function MicSection({
  available,
  enabled,
  error,
  streaming,
  onChange,
}: {
  available: boolean;
  enabled: boolean;
  error: string | null;
  // Whether the remote is capturing right now — an application over there has
  // the microphone open. Worded rather than implied, as with the camera.
  streaming: boolean;
  onChange: (enabled: boolean) => void;
}) {
  if (!available) {
    return null;
  }
  return (
    <div className="toolbar-section">
      <span className="toolbar-label">Microphone</span>
      <button
        type="button"
        className="toolbar-btn"
        onClick={() => onChange(!enabled)}
        aria-pressed={enabled}
        title="Offer this browser's microphone to the remote"
      >
        {enabled ? "Disable microphone" : "Enable microphone"}
      </button>
      {enabled && !error && (
        <p className="audio-note">
          {streaming
            ? "The remote is using the microphone"
            : "Waiting for the remote to open the microphone"}
        </p>
      )}
      {error && <p className="audio-note">{error}</p>}
    </div>
  );
}

// macOS-only Command-to-Control preference. It remains visible but inactive for
// a Mac guest, where Command already has native meaning.
function MacKeyboardSection({
  enabled,
  active,
  isMacHost,
  remoteIsMac,
  onChange,
}: {
  enabled: boolean;
  active: boolean;
  isMacHost: boolean;
  remoteIsMac: boolean;
  onChange: (enabled: boolean) => void;
}) {
  if (!isMacHost) {
    return null;
  }
  // All three states named the same way, with the Help card carrying what the
  // chord table used to try to say in a button's width. "n/a" rather than "off"
  // for a Mac guest: the preference may well be on, and it is the guest that
  // makes it inapplicable.
  const label = remoteIsMac
    ? "Mac key override: n/a"
    : active
      ? "Mac key override: on"
      : "Mac key override: off";
  return (
    <div className="toolbar-section">
      <span className="toolbar-label">Mac keyboard</span>
      <button
        type="button"
        className="toolbar-btn"
        onClick={() => onChange(!enabled)}
        aria-pressed={active}
        disabled={remoteIsMac}
        title={
          remoteIsMac
            ? "This remote is a Mac, so Command chords are sent as Command"
            : "Send ⌘A ⌘C ⌘F ⌘P ⌘S ⌘V ⌘X ⌘Z to the remote as Ctrl chords, and a bare ⌘ as the Windows key. Your browser keeps ⌘W, ⌘T, ⌘N, ⌘L, ⌘O and ⌘R for itself."
        }
      >
        {label}
      </button>
    </div>
  );
}

export default function FloatingMenu({
  onLogout,
  onSwitchTarget,
  sendKeyCombo,
  onKeyboardInset,
  canClipboard,
  remoteClipboard,
  onFetchClipboard,
  onSendClipboard,
  displays,
  activeDisplayId,
  onSelectDisplay,
  size,
  hostScale,
  connection,
  renderPlan,
  canAudio,
  audioEnabled,
  audioError,
  audioStream,
  videoStreams,
  onAudioChange,
  canCamera,
  cameraEnabled,
  cameraError,
  cameraStreaming,
  onCameraChange,
  canMic,
  micEnabled,
  micError,
  micStreaming,
  onMicChange,
  macKeyOverridesEnabled,
  macKeyOverridesActive,
  isMacHost,
  remoteIsMac,
  onMacKeyOverridesChange,
  onLocalShortcut,
}: {
  onLogout: () => void;
  // Return to the post-login target picker ("switch target"): disconnects the
  // current session without ending the login. See useRemoteDesktop.
  onSwitchTarget: () => void;
  sendKeyCombo: (codes: string[]) => void;
  // Reports the open docked panel's height so the touch canvas can inset above
  // it (0 when the panel closes or floats). See useRemoteDesktop. Both panels
  // share this channel, which is safe because only one is ever open.
  onKeyboardInset: (px: number) => void;
  // Whether the connected target opted into the clipboard bridge
  // (`clipboard = true`). False leaves the Clipboard button disabled.
  canClipboard: boolean;
  // The last clipboard reply from the server, and the fetch actions. See
  // ClipboardPanel — the browser holds no clipboard state of its own.
  // `onFetchClipboard` resolves with the remote snapshot, or null if nothing
  // answered.
  remoteClipboard: RemoteClipboard | null;
  onFetchClipboard: () => Promise<ClipboardSnapshot | null>;
  onSendClipboard: (text: string) => void;
  // The remote's displays and the one it is sharing. Empty for every engine
  // that cannot offer a choice, which is what hides the section — a list of one
  // hides it too, since there would be nothing to switch to. See DisplayPanel.
  displays: DisplayInfo[];
  activeDisplayId: number | null;
  onSelectDisplay: (id: number) => void;
  // The remote's framebuffer and its density, and this screen's density — the
  // read-only Screen section, shown for every target. See ScreenHelp for why
  // both densities and not just the size.
  size: RemoteSize | null;
  hostScale: number;
  // What this session is speaking, one line, from `connected` — the protocol and
  // the target's subtype where it has one. See connectionLabel.ts.
  connection: string;
  // The render dial this session resolved to, one line, from `connected`.
  renderPlan: string;
  // Whether this session can carry the remote's sound, which hides the Audio
  // section rather than disabling it — the same rule the Display section follows
  // and the opposite of Clipboard's. A greyed "Audio" would be explaining a
  // feature that does not exist for this target: audio is RDP-only, so on VNC
  // there is nothing that could be switched on.
  //
  // `audioEnabled` is what this browser has asked for, not proof that sound is
  // arriving: a quiet remote and one that will never redirect are the same thing
  // from the gateway's end. `audioError` is the one thing worth reporting — a
  // browser that cannot decode Opus. See useRemoteDesktop and audioPlayer.ts.
  canAudio: boolean;
  audioEnabled: boolean;
  audioError: string | null;
  // The two the card reads and the drawer does not: what the sound turned out to
  // be, and what each video decoder was configured with. Both null/empty until a
  // format arrives, which is a state the card words rather than hides.
  audioStream: AudioStreamInfo | null;
  videoStreams: readonly string[];
  onAudioChange: (enabled: boolean) => void;
  // The camera, under Audio's hide-don't-disable rule: `camera = true` is
  // RDP-only, so on every other target there is nothing that could be switched
  // on. `cameraEnabled` is per session and never remembered — see
  // useRemoteDesktop — and `cameraStreaming` is whether the remote is
  // consuming, which is the half the camera light cannot say.
  canCamera: boolean;
  cameraEnabled: boolean;
  cameraError: string | null;
  cameraStreaming: boolean;
  onCameraChange: (enabled: boolean) => void;
  // The microphone, the camera's twin under the same hide-don't-disable rule:
  // `microphone = true` is RDP-only. `micEnabled` is per session and never
  // remembered, and `micStreaming` is whether the remote has it open.
  canMic: boolean;
  micEnabled: boolean;
  micError: string | null;
  micStreaming: boolean;
  onMicChange: (enabled: boolean) => void;
  // The Command-to-Control preference and whether it is doing anything. The two
  // differ when the guest is itself a Mac, which is why the section reports the
  // reason rather than just showing the switch off. The whole section is absent
  // on a non-Mac host, where there is no Command key to translate. See macKeys.ts.
  macKeyOverridesEnabled: boolean;
  macKeyOverridesActive: boolean;
  isMacHost: boolean;
  remoteIsMac: boolean;
  onMacKeyOverridesChange: (enabled: boolean) => void;
  // A chord this component took for itself, announced to the input path so it can
  // unwind what it was holding for one. Only the Mac spelling of the chrome
  // shortcut needs it, and only because Command is in it. See useRemoteDesktop.
  onLocalShortcut: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [helpOpen, setHelpOpen] = useState(false);
  const { panel, setPanel, closePanel, togglePanel } = usePanel();
  // True between pressing Clipboard and the remote's text arriving. The panel
  // stays closed for that moment so it never opens on stale text that visibly
  // rewrites itself a beat later.
  const [clipboardPending, setClipboardPending] = useState(false);
  // null = not yet moved; resolvedPosition falls back to the top-right corner.
  const [position, setPosition] = useState<Position | null>(null);
  const [dragging, setDragging] = useState(false);
  const [viewport, setViewport] = useState<Viewport>(readViewport);

  const dragStateRef = useRef<DragState | null>(null);
  // A drag ends with a synthetic click on some platforms; swallow it so a drag
  // never toggles the toolbar.
  const suppressClickRef = useRef(false);

  useEffect(() => {
    const update = () => {
      const next = readViewport();
      setViewport((prev) =>
        prev.width === next.width &&
        prev.height === next.height &&
        prev.offsetX === next.offsetX &&
        prev.offsetY === next.offsetY
          ? prev
          : next,
      );
    };
    window.addEventListener("resize", update);
    const vp = window.visualViewport;
    vp?.addEventListener("resize", update);
    vp?.addEventListener("scroll", update);
    return () => {
      window.removeEventListener("resize", update);
      vp?.removeEventListener("resize", update);
      vp?.removeEventListener("scroll", update);
    };
  }, []);

  const clamp = useCallback(
    (x: number, y: number): Position => {
      const minX = viewport.offsetX + FAB_MARGIN;
      const minY = viewport.offsetY + FAB_MARGIN;
      const maxX =
        viewport.offsetX +
        Math.max(FAB_MARGIN, viewport.width - FAB_SIZE - FAB_MARGIN);
      const maxY =
        viewport.offsetY +
        Math.max(FAB_MARGIN, viewport.height - FAB_SIZE - FAB_MARGIN);
      return {
        x: Math.min(Math.max(x, minX), maxX),
        y: Math.min(Math.max(y, minY), maxY),
      };
    },
    [viewport],
  );

  const defaultPosition = useCallback(
    (): Position =>
      clamp(
        viewport.offsetX + viewport.width - FAB_SIZE - FAB_MARGIN,
        viewport.offsetY + FAB_MARGIN,
      ),
    [clamp, viewport],
  );

  const resolvedPosition = useMemo(
    () => position ?? defaultPosition(),
    [position, defaultPosition],
  );

  // A shrinking viewport (rotation, keyboard) can strand the FAB off-screen;
  // re-clamp whatever position is held.
  useEffect(() => {
    setPosition((prev) => (prev ? clamp(prev.x, prev.y) : prev));
  }, [clamp]);

  // Escape dismisses the gesture-help overlay, matching the backdrop tap and
  // the Close button. Listener lives only while the overlay is open.
  useEffect(() => {
    if (!helpOpen) {
      return;
    }
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setHelpOpen(false);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [helpOpen]);

  // Capture the non-persisted chrome shortcut before remote input forwarding.
  const [hidden, setHidden] = useState(false);
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      // The host's own middle modifier and pointedly not the other one, which is
      // what leaves the four-modifier hyper chord to its owner. See
      // hideChromeShortcut.
      const middle = isMacHost
        ? e.metaKey && !e.altKey
        : e.altKey && !e.metaKey;
      if (e.code !== "Semicolon" || !e.ctrlKey || !e.shiftKey || !middle) {
        return;
      }
      e.preventDefault();
      e.stopPropagation();
      // Command is held and the key it was held with has just been taken by this
      // client, so the input path never saw the chord and would read Command's
      // release as a bare tap — which is how the guest's Start menu opens. Hiding
      // a button must not do that. See macKeys.ts.
      if (isMacHost) {
        onLocalShortcut();
      }
      setHidden((was) => !was);
    };
    window.addEventListener("keydown", onKeyDown, { capture: true });
    return () =>
      window.removeEventListener("keydown", onKeyDown, { capture: true });
  }, [isMacHost, onLocalShortcut]);

  const onPointerDown = useCallback(
    (e: ReactPointerEvent<HTMLButtonElement>) => {
      if (
        e.button !== 0 &&
        e.pointerType !== "touch" &&
        e.pointerType !== "pen"
      ) {
        return;
      }
      const current = position ?? defaultPosition();
      dragStateRef.current = {
        pointerId: e.pointerId,
        startX: e.clientX,
        startY: e.clientY,
        originX: current.x,
        originY: current.y,
        dragged: false,
      };
      e.currentTarget.setPointerCapture(e.pointerId);
    },
    [position, defaultPosition],
  );

  const onPointerMove = useCallback(
    (e: ReactPointerEvent<HTMLButtonElement>) => {
      const drag = dragStateRef.current;
      if (!drag || drag.pointerId !== e.pointerId) {
        return;
      }
      const dx = e.clientX - drag.startX;
      const dy = e.clientY - drag.startY;
      if (!drag.dragged && Math.hypot(dx, dy) >= DRAG_THRESHOLD) {
        drag.dragged = true;
        setDragging(true);
      }
      if (!drag.dragged) {
        return;
      }
      setPosition(clamp(drag.originX + dx, drag.originY + dy));
      suppressClickRef.current = true;
      e.preventDefault();
    },
    [clamp],
  );

  const endDrag = useCallback((pointerId: number) => {
    const drag = dragStateRef.current;
    if (!drag || drag.pointerId !== pointerId) {
      return;
    }
    dragStateRef.current = null;
    setDragging(false);
    if (drag.dragged) {
      // Touch may never fire the click that clears the guard; drop it on a
      // timer so the next tap isn't swallowed.
      setTimeout(() => {
        suppressClickRef.current = false;
      }, 400);
    }
  }, []);

  const onPointerUp = useCallback(
    (e: ReactPointerEvent<HTMLButtonElement>) => endDrag(e.pointerId),
    [endDrag],
  );
  const onPointerCancel = useCallback(
    (e: ReactPointerEvent<HTMLButtonElement>) => endDrag(e.pointerId),
    [endDrag],
  );

  const onClick = useCallback(() => {
    if (suppressClickRef.current) {
      suppressClickRef.current = false;
      return;
    }
    setOpen((prev) => !prev);
  }, []);

  // Open the on-screen keyboard and collapse the drawer so the panel has the
  // screen to itself; toggling the button again closes the panel.
  const onSoftKeyboard = useCallback(() => {
    togglePanel("keyboard");
    setOpen(false);
  }, [togglePanel]);

  // A button gesture in Chrome's app window requests the remote's point-size
  // viewport plus whatever frame Chrome and this OS currently put around it.
  // Close the drawer first so a smaller requested window does not leave its menu
  // covering the desktop it was just sized to show.
  const onSizeWindow = useCallback(() => {
    if (!size) {
      return;
    }
    setOpen(false);
    sizeWindowToDesktop(size, size.scale);
  }, [size]);

  // Same deal for the clipboard panel, except it cannot open straight away: it
  // fetches first and waits for the answer, so it appears already showing what
  // the remote holds right now. Without that it would open on whatever arrived
  // last — which is nothing at all for a browser that attached mid-session,
  // since it missed every push that came before it.
  const onClipboard = useCallback(() => {
    setOpen(false);
    if (panel === "clipboard") {
      closePanel();
      return;
    }
    if (clipboardPending) {
      return; // a second press while the first is still in flight
    }
    const panelAtFetchStart = panel;
    setClipboardPending(true);
    void onFetchClipboard().finally(() => {
      setClipboardPending(false);
      // Opened even when nothing answered: the panel reports the empty result
      // and its own Fetch is right there to retry.
      setPanel((current) =>
        current === panelAtFetchStart ? "clipboard" : current,
      );
    });
  }, [panel, clipboardPending, onFetchClipboard, closePanel, setPanel]);

  // The drawer anchors to the FAB: right-aligned to it, placed below unless the
  // FAB sits too low, in which case it flips above.
  const toolbarStyle = useMemo(() => {
    const minLeft = viewport.offsetX + FAB_MARGIN;
    const maxLeft =
      viewport.offsetX +
      Math.max(FAB_MARGIN, viewport.width - TOOLBAR_WIDTH - FAB_MARGIN);
    const desiredLeft = resolvedPosition.x + FAB_SIZE - TOOLBAR_WIDTH;
    const left = Math.min(Math.max(desiredLeft, minLeft), maxLeft);

    const topBelow = resolvedPosition.y + FAB_SIZE + TOOLBAR_GAP;
    const topAbove = resolvedPosition.y - TOOLBAR_GAP;
    const availableBelow =
      viewport.offsetY + viewport.height - topBelow - FAB_MARGIN;
    const availableAbove = topAbove - viewport.offsetY - FAB_MARGIN;
    const placeBelow =
      availableBelow >= TOOLBAR_MIN_HEIGHT || availableBelow >= availableAbove;
    const maxHeight = Math.max(
      TOOLBAR_MIN_HEIGHT,
      Math.floor(placeBelow ? availableBelow : availableAbove),
    );

    return placeBelow
      ? { left: `${left}px`, top: `${topBelow}px`, maxHeight: `${maxHeight}px` }
      : {
          left: `${left}px`,
          top: `${topAbove}px`,
          transform: "translateY(-100%)",
          maxHeight: `${maxHeight}px`,
        };
  }, [resolvedPosition, viewport]);

  return (
    <>
      {/* The button and its drawer go together: a toolbar anchored to a button
          that isn't there reads as a bug. Both keep their state while hidden, so
          the chord brings back exactly what was on screen. Docked panels are left
          alone because they carry their own Close. */}
      {!hidden && (
        <button
          type="button"
          className={`fab${open ? " fab-open" : ""}${dragging ? " fab-dragging" : ""}`}
          style={{
            left: `${resolvedPosition.x}px`,
            top: `${resolvedPosition.y}px`,
          }}
          onClick={onClick}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerCancel}
          aria-label={open ? "Close menu" : "Open menu"}
          aria-expanded={open}
          // The only place the chord is written down in the UI, and it has to be
          // here: once the button is hidden there is nothing left to read it off.
          title={`${hideChromeShortcut(isMacHost)} hides this button`}
        >
          {open ? "✕" : "☰"}
        </button>
      )}

      {open && !hidden && (
        <div className="toolbar" style={toolbarStyle}>
          <WindowSection size={size} onSize={onSizeWindow} />

          <DisplaySection
            displays={displays}
            activeDisplayId={activeDisplayId}
            open={panel === "display"}
            onToggle={() => {
              setOpen(false);
              togglePanel("display");
            }}
          />

          <div className="toolbar-section">
            <span className="toolbar-label">Clipboard</span>
            {/* Enabled per target (`clipboard = true`), which every protocol
                supports; a target that didn't opt in leaves this disabled. */}
            <button
              type="button"
              className="toolbar-btn"
              onClick={onClipboard}
              disabled={!canClipboard || clipboardPending}
              aria-pressed={panel === "clipboard"}
              aria-busy={clipboardPending}
              title={
                canClipboard
                  ? "Read and write the remote's clipboard"
                  : "Clipboard sync is not enabled for this target"
              }
            >
              {clipboardPending
                ? "Fetching…"
                : panel === "clipboard"
                  ? "Hide clipboard"
                  : "Clipboard"}
            </button>
          </div>

          <AudioSection
            available={canAudio}
            enabled={audioEnabled}
            error={audioError}
            onChange={onAudioChange}
          />

          <CameraSection
            available={canCamera}
            enabled={cameraEnabled}
            error={cameraError}
            streaming={cameraStreaming}
            onChange={onCameraChange}
          />

          <MicSection
            available={canMic}
            enabled={micEnabled}
            error={micError}
            streaming={micStreaming}
            onChange={onMicChange}
          />

          <MacKeyboardSection
            enabled={macKeyOverridesEnabled}
            active={macKeyOverridesActive}
            isMacHost={isMacHost}
            remoteIsMac={remoteIsMac}
            onChange={onMacKeyOverridesChange}
          />

          <div className="toolbar-section toolbar-actions">
            <button
              type="button"
              className="toolbar-btn"
              onClick={() => setHelpOpen(true)}
              title="This session's size, density, render dial and decoders, and the touch gestures"
            >
              Help
            </button>
            <button
              type="button"
              className="toolbar-btn"
              onClick={onSoftKeyboard}
              aria-pressed={panel === "keyboard"}
            >
              {panel === "keyboard" ? "Hide keyboard" : "Soft keyboard"}
            </button>
            <button
              type="button"
              className="toolbar-btn"
              onClick={onSwitchTarget}
              title="Disconnect and return to the target picker"
            >
              Switch target
            </button>
            <button
              type="button"
              className="toolbar-btn toolbar-btn-danger"
              onClick={onLogout}
            >
              Log out
            </button>
          </div>
        </div>
      )}

      {helpOpen && (
        // biome-ignore lint/a11y/useKeyWithClickEvents: tap-outside dismiss; the Close button covers keyboard users
        // biome-ignore lint/a11y/noStaticElementInteractions: overlay backdrop
        <div className="help-overlay" onClick={() => setHelpOpen(false)}>
          {/* biome-ignore lint/a11y/useKeyWithClickEvents: inner card only stops the backdrop's dismiss */}
          {/* biome-ignore lint/a11y/noStaticElementInteractions: inner card */}
          <div className="help-card" onClick={(e) => e.stopPropagation()}>
            <h2>Help</h2>
            <ScreenHelp
              size={size}
              hostScale={hostScale}
              connection={connection}
              renderPlan={renderPlan}
              audio={{
                available: canAudio,
                enabled: audioEnabled,
                error: audioError,
                stream: audioStream,
              }}
              videoStreams={videoStreams}
            />
            <h3>Shortcuts</h3>
            <dl className="help-list">
              <div className="help-item">
                <dt>Hide or show this menu</dt>
                {/* Worth documenting precisely because of what it does: once the
                    ☰ button is hidden there is nothing left on screen to read the
                    way back off, so a shortcut nobody wrote down is a menu that
                    looks gone for good. */}
                <dd>{hideChromeShortcut(isMacHost)}</dd>
              </div>
              <AppWindowHelpRow />
            </dl>
            {isMacHost && (
              <>
                <h3>Mac key override</h3>
                <dl className="help-list">
                  {MAC_KEY_HELP.map((row) => (
                    <div key={row.situation} className="help-item">
                      <dt>{row.situation}</dt>
                      <dd>{row.effect}</dd>
                    </div>
                  ))}
                </dl>
              </>
            )}
            <h3>Touch gestures</h3>
            <dl className="help-list">
              {GESTURE_HELP.map((row) => (
                <div key={row.gesture} className="help-item">
                  <dt>{row.gesture}</dt>
                  <dd>{row.action}</dd>
                </div>
              ))}
            </dl>
            <button
              type="button"
              className="toolbar-btn"
              onClick={() => setHelpOpen(false)}
            >
              Close
            </button>
          </div>
        </div>
      )}

      <DockedPanel
        panel={panel}
        onClose={closePanel}
        onDockedHeightChange={onKeyboardInset}
        sendKeyCombo={sendKeyCombo}
        remoteClipboard={remoteClipboard}
        onSendClipboard={onSendClipboard}
        displays={displays}
        activeDisplayId={activeDisplayId}
        onSelectDisplay={onSelectDisplay}
      />
    </>
  );
}
