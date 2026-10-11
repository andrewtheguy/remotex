import {
  type ReactNode,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import { AppVersion } from "./AppVersion.tsx";
import { appWindow, onAppWindowChange } from "./appWindow.ts";
import { ClipboardPanel } from "./ClipboardPanel.tsx";
import DisplayPanel, { DisplayTabLink } from "./DisplayPanel.tsx";
import { desktopViewportSize, sizeWindowToDesktop } from "./desktopWindow.ts";
import {
  FloatingButton,
  hideChromeShortcut,
  useFloatingButton,
} from "./floatingButton.tsx";
import {
  fullscreenSupported,
  isFullscreen,
  onFullscreenChange,
  toggleFullscreen,
} from "./fullscreen.ts";
import { keyboardLockSupported } from "./keyboardLock.ts";
import {
  type AudioBlock,
  type AudioRow,
  type AudioStreamInfo,
  audioBlockCopy,
  audioLabel,
  renderLabel,
  type VideoStreamInfo,
  videoLabel,
} from "./mediaLabel.ts";
import { keepTabWithin } from "./modalFocus.ts";
import type {
  ClipboardSnapshot,
  DisplayInfo,
  HoldCause,
  RemoteClipboard,
} from "./protocol.ts";
import { SoftKeyboardPanel } from "./SoftKeyboardPanel.tsx";
import ThroughputPanel, { useThroughputAvailable } from "./ThroughputPanel.tsx";
import {
  CAN_PINCH_ZOOM,
  densityLabel,
  type RemoteSize,
} from "./useRemoteDesktop.ts";

// The floating chrome — a draggable ☰ button that toggles a toolbar drawer. The
// drawer carries this project's controls, an End session button that returns to
// the post-login picker, and Log out, which ends the web login. Three of its
// buttons open a panel instead of acting: Soft keyboard — which is where every
// key, modifier and browser-swallowed combo now lives — Clipboard (see
// ClipboardPanel), and Display, only for a remote
// that offers more than one (see DisplayPanel). The button and where the drawer
// goes are floatingButton.tsx's, shared with a second display's tab.

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

// What this keyboard's modifiers become on a Mac remote, mirroring
// altAsCommand.ts. The mirror of the table above in every sense: it is shown
// only on a host with no Command key of its own, and only against a Mac.
const ALT_AS_COMMAND_HELP: readonly { situation: string; effect: string }[] = [
  { situation: "Left Alt", effect: "Arrives as ⌘, so Alt+C copies" },
  { situation: "Windows key", effect: "Arrives as ⌘" },
  { situation: "Right Alt", effect: "Arrives as ⌥" },
  { situation: "Left ⌥", effect: "Not on this keyboard — use the right Alt" },
];

// The touch gesture cheat-sheet, mirroring touchGestures.ts.
const GESTURE_HELP: readonly { gesture: string; action: string }[] = [
  { gesture: "Tap", action: "Left-click" },
  { gesture: "Double-tap and hold", action: "Grab, then drag" },
  { gesture: "One-finger drag", action: "Move cursor + pan" },
  { gesture: "Two-finger tap", action: "Right-click" },
  { gesture: "Two-finger pinch", action: "Zoom" },
  { gesture: "Two-finger swipe", action: "Scroll" },
];

// Which docked panel is open, if any.
//
// One state rather than a boolean each: both dock to the bottom edge and report
// the same canvas inset, so a second one open would sit on the first. Two
// booleans made that a rule every call site had to remember — open this one,
// clear the other — and this makes it impossible to express.
type Panel = "clipboard" | "keyboard" | "display";

// Which face the one modal card shows, when it is up at all: what this session
// is, how to drive it, or what it has carried.
type Modal = "info" | "help" | "throughput";

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

/// What the mode is worth and how to get out of it, which are both easy to get wrong
/// from the outside: Chrome's own full screen looks identical and does none of it, and
/// Escape leaves by being held rather than pressed once it is locked with the rest.
function ImmersiveHelpRows() {
  if (!fullscreenSupported()) {
    return null;
  }
  // A full screen without Keyboard Lock is the larger desktop and nothing more, so
  // the card promises the keys only where the browser has them to give. Escape also
  // stops being a key that has to be *held*, which is a lock's doing alone.
  const locks = keyboardLockSupported();
  return (
    <>
      <div className="help-item">
        <dt>
          {locks
            ? "Send every key, Super and Alt+Tab included"
            : "Fill the screen with the remote desktop"}
        </dt>
        <dd>
          {locks
            ? "Menu → Immersive full screen. Chrome's own full screen — ⛶ beside the zoom row, or F11 — looks the same and does not do this"
            : "Menu → Full screen. This browser has no Keyboard Lock, so Super, Alt+Tab and the browser's own chords stay with this computer"}
        </dd>
      </div>
      <div className="help-item">
        <dt>{locks ? "Leave immersive full screen" : "Leave full screen"}</dt>
        <dd>
          {locks
            ? "Hold Esc, or the same menu button"
            : "Esc, or the same menu button"}
        </dd>
      </div>
    </>
  );
}

// The backdrop and card every modal shares. The card is a modal dialog: focus moves
// into it when it opens and Tab stays inside it. Escape dismisses it, matching the
// backdrop tap and the card's own Close; the listener lives only while it is mounted.
function ModalOverlay({
  label,
  className,
  onDismiss,
  children,
}: {
  label: string;
  className: string;
  onDismiss: () => void;
  children: ReactNode;
}) {
  const cardRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    cardRef.current?.focus();
  }, []);
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        onDismiss();
      } else if (e.key === "Tab") {
        keepTabWithin(cardRef.current, document.activeElement, e);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onDismiss]);
  return (
    // biome-ignore lint/a11y/useKeyWithClickEvents: tap-outside dismiss; the Close button covers keyboard users
    // biome-ignore lint/a11y/noStaticElementInteractions: overlay backdrop
    <div className="help-overlay" onClick={onDismiss}>
      {/* biome-ignore lint/a11y/useKeyWithClickEvents: inner card only stops the backdrop's dismiss */}
      <div
        ref={cardRef}
        className={className}
        role="dialog"
        aria-modal="true"
        aria-label={label}
        tabIndex={-1}
        onClick={(e) => e.stopPropagation()}
      >
        {children}
      </div>
    </div>
  );
}

// The Info card's way into "Throughput", offered only on a gateway with `[meter].enabled`.
function ThroughputButton({ onOpen }: { onOpen: () => void }) {
  if (!useThroughputAvailable()) {
    return null;
  }
  return (
    <button type="button" className="toolbar-btn" onClick={onOpen}>
      Throughput
    </button>
  );
}

// The Info card switched to "Throughput". Back returns to Info; the backdrop and
// Escape close both.
function ThroughputModal({
  open,
  onBack,
  onDismiss,
  onUnauthorized,
}: {
  open: boolean;
  onBack: () => void;
  onDismiss: () => void;
  onUnauthorized: () => void;
}) {
  if (!open) {
    return null;
  }
  return (
    <ModalOverlay
      label="Throughput"
      className="help-card throughput-card"
      onDismiss={onDismiss}
    >
      <ThroughputPanel
        closeLabel="Back to info"
        onClose={onBack}
        onUnauthorized={onUnauthorized}
      />
    </ModalOverlay>
  );
}

/// Reports to the desktop that it is view-only for as long as this menu has
/// something over it — the drawer, the modal card that opens from it and leaves
/// the drawer standing, or the clipboard panel, which is read, typed into and
/// copied from like a card and has no more use for a live desktop behind it than
/// one. The other docked panels are the desktop's own controls and leave it live.
/// Turning it off again is the effect's cleanup, so there is no
/// path where the menu goes away and the desktop stays inert: unmounting the menu
/// hands the input back too. The chord that hides the ☰ button takes the drawer with
/// it, which is why the drawer's own state is not the whole answer.
function useViewOnly(
  drawerOpen: boolean,
  chromeHidden: boolean,
  modal: Modal | null,
  panel: Panel | null,
  report: (viewOnly: boolean) => void,
) {
  const menuUp = (drawerOpen && !chromeHidden) || modal !== null;
  const anythingUp = menuUp || panel === "clipboard";
  useEffect(() => {
    if (!anythingUp) {
      return;
    }
    report(true);
    return () => report(false);
  }, [anythingUp, report]);
  if (!anythingUp) {
    return null;
  }
  return menuUp ? "menu" : "clipboard";
}

// The menu is over the desktop, so the desktop is a picture of itself for as long
// as that lasts: it keeps painting and takes no input at all (see
// useRemoteDesktop), and says which of the two it is doing. Takes the pointer
// rather than passing it through — the surface underneath hides the browser's own
// cursor, and a menu is no place to be without one — and a click on it closes the
// drawer and the clipboard panel, as one beside any menu does.
export function ViewOnlyCover({
  over,
  onDismiss,
}: {
  // What stands over the desktop, null where nothing does.
  over: "menu" | "clipboard" | null;
  onDismiss: () => void;
}) {
  if (!over) {
    return null;
  }
  return (
    // biome-ignore lint/a11y/useKeyWithClickEvents: click-outside dismiss; the ✕ of the drawer and of the panel cover keyboard users
    // biome-ignore lint/a11y/noStaticElementInteractions: the cover behind the drawer
    <div className="view-only" onClick={onDismiss}>
      <span className="view-only-label">
        View only while the {over} is open
      </span>
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
  onFocusDesktop,
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
  onFocusDesktop: () => void;
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
          onFocusDesktop={onFocusDesktop}
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
      {/* A display shown in a tab of its own is one click from the drawer: All
          Displays is what a session of two starts on, and the other display is
          not on screen until its tab is opened. The link the picker has. */}
      {displays.map(
        (display) =>
          display.tab !== null && (
            <DisplayTabLink
              key={display.id}
              tab={display.tab}
              className="toolbar-btn toolbar-link"
              title={`Show ${display.label} in a browser tab of its own`}
            >
              Open {display.label} ↗
            </DisplayTabLink>
          ),
      )}
    </div>
  );
}

// Immersive full screen — and the only reason this client carries a full-screen control
// at all. Chromium activates Keyboard Lock in the page's own full screen and in no
// other, so Chrome's ⛶ (the one beside the zoom row, and F11) hides the frame while the
// host goes on taking the Super key, Alt+Tab and the browser's own chords out of the
// stream: the remote desktop fills the screen and Super+E still opens a local window.
// Nothing the page can do promotes that full screen into this one, which is why this is
// a button rather than something the client arranges for itself. See fullscreen.ts.
//
// Offered wherever the browser grants it, touch clients included. A phone has no Super
// key to win back, but full screen is still the larger desktop.
export function FullscreenSection({ onSettled }: { onSettled: () => void }) {
  const fullscreen = useSyncExternalStore(
    onFullscreenChange,
    isFullscreen,
    () => false,
  );
  // A request refused for want of a user gesture, or by a permissions policy, changes
  // nothing on screen. Said out loud here, because the alternative is a button that
  // looks broken.
  const [refusal, setRefusal] = useState<string | null>(null);
  if (!fullscreenSupported()) {
    return null;
  }
  // Whether this browser can hand over the keys as well as the screen. A mode called
  // immersive that still loses Super to the host would be the same puzzle Chrome's own
  // full screen already is, so it is only called that where the lock exists.
  const locks = keyboardLockSupported();
  return (
    <div className="toolbar-section">
      <span className="toolbar-label">Full screen</span>
      <button
        type="button"
        className="toolbar-btn"
        onClick={() => {
          setRefusal(null);
          toggleFullscreen().then(onSettled, (cause: unknown) =>
            setRefusal(cause instanceof Error ? cause.message : String(cause)),
          );
        }}
        aria-pressed={fullscreen}
        title={
          fullscreen
            ? "Return to the window; the browser and this computer take their shortcuts back"
            : locks
              ? "Fill the screen and let Super, Alt+Tab and the browser's own chords reach the remote. Hold Esc to leave"
              : "Fill the screen with the remote desktop. This browser has no Keyboard Lock, so Super, Alt+Tab and the browser's own chords stay with this computer. Esc leaves"
        }
      >
        {fullscreen
          ? "Leave full screen"
          : locks
            ? "Immersive full screen"
            : "Full screen"}
      </button>
      {refusal && <p className="toolbar-note">{refusal}</p>}
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

// A remote display's pixels and the density it draws them at, as the Info card says
// it of the display on its page and a second display's menu says it of its own.
export function remoteSizeLabel(size: RemoteSize): string {
  return `${size.w}×${size.h} at ${densityLabel(size.scale * 100)} (${dpiLabel(size.scale * 100)})`;
}

// What the remote is drawing against what this browser is, at the top of the Info
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
  oversize,
  audio,
  videoStream,
}: {
  size: RemoteSize | null;
  hostScale: number;
  connection: string;
  renderPlan: string;
  oversize: HoldCause | null;
  audio: AudioRow;
  videoStream: VideoStreamInfo | null;
}) {
  const video = videoLabel(videoStream, oversize);
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
            {size ? remoteSizeLabel(size) : "Waiting for the remote desktop"}
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
          <dd>{renderLabel(renderPlan)}</dd>
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
        <div className="help-item">
          <dt>Video decoder</dt>
          {/* The exact WebCodecs configuration the decoder was built with. It is
              what a `VideoDecoder` complaint names, and until this row it was
              readable only in the console — on a session that is *working*, not
              one that failed, which is when the question is usually asked. With
              it, whether the stream is the remote's own passed through or one
              the gateway encoded, which only the gateway knows. */}
          <dd>{video}</dd>
        </div>
      </dl>
    </>
  );
}

// The displays of a session that shows one in a tab of its own, which is All
// Displays on a target with two: the one on this page, and each other with the link
// that opens it, the same one the Display picker has. It names them and nothing
// more: what this page's is drawn at is the Remote desktop row above, and what
// another's is drawn at is known to the tab showing it, whose menu says so. Absent on
// every other session, where the Display picker is the whole of it.
function DisplaysHelp({ shown }: { shown: DisplayInfo[] }) {
  if (shown.length === 0) {
    return null;
  }
  return (
    <>
      <h3>Displays</h3>
      <dl className="help-list">
        {shown.map((display) => (
          <div key={display.id} className="help-item">
            <dt>
              {display.label}
              {display.tab === null ? " (Current)" : ""}
            </dt>
            <dd>
              {display.tab === null ? (
                "On this page"
              ) : (
                <DisplayTabLink tab={display.tab}>
                  Open in a new tab ↗
                </DisplayTabLink>
              )}
            </dd>
          </div>
        ))}
      </dl>
    </>
  );
}

// The displays a session shows at once, one on this page and the rest in tabs of
// their own: every display but the chosen entry, which is All Displays itself.
// Empty on a session that shows one display, whatever the picker lists.
function displaysShown(
  displays: DisplayInfo[],
  activeDisplayId: number | null,
): DisplayInfo[] {
  if (!displays.some((display) => display.tab !== null)) {
    return [];
  }
  return displays.filter((display) => display.id !== activeDisplayId);
}

// The direct audio toggle is also the user gesture required to create a
// playable AudioContext. A session without sound omits the row. Its words are
// Mute and Unmute because that is all it reaches: whether the remote's sound is
// taken was chosen at the picker before the session started, and this opens or
// closes the browser's subscription to it.
//
// Sound this browser cannot play keeps the row, greyed, saying why in a few words:
// a browser with no audio decoder in every session (the picker greyed its sound
// too), and a High Performance Mac's AAC-ELD where this browser decodes neither of
// its forms.
function AudioSection({
  available,
  blocked,
  enabled,
  error,
  onChange,
}: {
  available: boolean;
  blocked: AudioBlock | null;
  enabled: boolean;
  error: string | null;
  onChange: (enabled: boolean) => void;
}) {
  if (blocked) {
    const copy = audioBlockCopy(blocked);
    return (
      <div className="toolbar-section">
        <span className="toolbar-label">Audio</span>
        <button
          type="button"
          className="toolbar-btn"
          disabled
          title={copy.reason}
        >
          {copy.button}
        </button>
      </div>
    );
  }
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
        {enabled ? "Mute" : "Unmute"}
      </button>
      {/* Quiet remotes have no distinct client-visible state. */}
      {error && <p className="audio-note">{error}</p>}
    </div>
  );
}

// The camera toggle is also the user gesture `getUserMedia`'s permission prompt
// requires. Targets without a camera omit the row — the same rule as Audio's.
// Unlike sound, no session is started with it: this button is the one and only
// way the camera turns on, per session, every session.
//
// The row says "experimental" because the redirection behind it has no automated
// coverage — no test carries a frame to a host — where Audio's does. The label
// is where an operator meets that, since it is the one place the feature is
// turned on.
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
      <span className="toolbar-label">Camera (experimental)</span>
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
// without `microphone = true` omits the row, the click is the `getUserMedia`
// gesture, nothing is remembered, and the label says "experimental" for the
// same lack of coverage.
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
  // Whether an application on the remote is recording right now.
  streaming: boolean;
  onChange: (enabled: boolean) => void;
}) {
  if (!available) {
    return null;
  }
  return (
    <div className="toolbar-section">
      <span className="toolbar-label">Microphone (experimental)</span>
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
            ? "The remote is recording the microphone"
            : "Waiting for the remote to record"}
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

// Touchscreen mode: fingers reach the remote as touch contacts and the guest
// reads the gestures itself — on Windows, its own tap, drag, press-and-hold,
// pinch, two-finger scroll and edge swipes. Shown only once the host has opened
// its touch channel on a device that has fingers to offer it; everywhere else
// the trackpad gestures are the only touch there is, and there is no switch to
// show. See touchPassthrough.ts.
function TouchscreenSection({
  offered,
  enabled,
  onChange,
}: {
  offered: boolean;
  enabled: boolean;
  onChange: (enabled: boolean) => void;
}) {
  if (!offered) {
    return null;
  }
  return (
    <div className="toolbar-section">
      <span className="toolbar-label">Touchscreen</span>
      <button
        type="button"
        className="toolbar-btn"
        onClick={() => onChange(!enabled)}
        aria-pressed={enabled}
        title={
          enabled
            ? "Fingers reach the remote as touch contacts: its own tap, drag, pinch, scroll and edge-swipe gestures apply. Turn off for the trackpad gestures."
            : "Send fingers to the remote as touch contacts, so its own gestures apply, instead of driving a cursor with the trackpad gestures."
        }
      >
        {enabled ? "Touchscreen: on" : "Touchscreen: off"}
      </button>
    </div>
  );
}

// Whichever of the two modifier tables this pairing has, and neither when a PC
// keyboard drives a PC: the translation is always one direction or the other,
// never both, so one heading stands for the whole of what the keys do.
function KeyboardHelp({
  isMacHost,
  remoteIsMac,
}: {
  isMacHost: boolean;
  remoteIsMac: boolean;
}) {
  let heading = "";
  let rows: readonly { situation: string; effect: string }[] = [];
  if (isMacHost) {
    heading = "Mac key override";
    rows = MAC_KEY_HELP;
  } else if (remoteIsMac) {
    heading = "Mac remote keys";
    rows = ALT_AS_COMMAND_HELP;
  } else {
    return null;
  }
  return (
    <>
      <h3>{heading}</h3>
      <dl className="help-list">
        {rows.map((row) => (
          <div key={row.situation} className="help-item">
            <dt>{row.situation}</dt>
            <dd>{row.effect}</dd>
          </div>
        ))}
      </dl>
    </>
  );
}

export default function FloatingMenu({
  onLogout,
  onUnauthorized,
  onSwitchTarget,
  sendKeyCombo,
  onKeyboardInset,
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
  oversize,
  canAudio,
  audioEnabled,
  audioBlock,
  audioError,
  audioStream,
  videoStream,
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
  touchOffered,
  touchEnabled,
  touchActive,
  onTouchChange,
  onLocalShortcut,
  onFocusDesktop,
  onViewOnlyChange,
  desktopShown,
}: {
  onLogout: () => void;
  // The throughput read came back 401: the login expired. See ThroughputPanel.
  onUnauthorized: () => void;
  // Return to the post-login target picker ("End session"): disconnects the
  // current session without ending the login. See useRemoteDesktop.
  onSwitchTarget: () => void;
  sendKeyCombo: (codes: string[]) => void;
  // Reports the open docked panel's height so the touch canvas can inset above
  // it (0 when the panel closes or floats). See useRemoteDesktop. Both panels
  // share this channel, which is safe because only one is ever open.
  onKeyboardInset: (px: number) => void;
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
  // Why the desktop has no picture, from `oversize`.
  oversize: HoldCause | null;
  // Whether this session carries the remote's sound, which hides the Audio
  // section rather than disabling it — the same rule the Display section follows
  // and the opposite of Clipboard's. A greyed "Audio" would be explaining sound
  // that is not there to unmute: a plain VNC server and a Mac in Standard mode
  // carry none, and a session started without it asked the remote for none. The
  // exception is `audioBlock`, sound this browser cannot play, where it is greyed
  // and says why (`AudioSection`).
  //
  // `audioEnabled` is what this browser has asked for, not proof that sound is
  // arriving: a quiet remote and one that will never redirect are the same thing
  // from the gateway's end. `audioError` is the one thing worth reporting — a
  // browser that cannot decode Opus. See useRemoteDesktop and audioPlayer.ts.
  canAudio: boolean;
  audioEnabled: boolean;
  audioBlock: AudioBlock | null;
  audioError: string | null;
  // The two the card reads and the drawer does not: what the sound turned out to
  // be, and what the video decoder was configured with. Both null until a format
  // arrives, which is a state the card words rather than hides.
  audioStream: AudioStreamInfo | null;
  videoStream: VideoStreamInfo | null;
  onAudioChange: (enabled: boolean) => void;
  // The camera, under Audio's hide-don't-disable rule: `camera = true` is
  // RDP's and a wlshare target's (wlshare's camera extension) alone, so on every other
  // target there is nothing that could be switched on. `cameraEnabled` is per
  // session and never remembered — see
  // useRemoteDesktop — and `cameraStreaming` is whether the remote is
  // consuming, which is the half the camera light cannot say.
  canCamera: boolean;
  cameraEnabled: boolean;
  cameraError: string | null;
  cameraStreaming: boolean;
  onCameraChange: (enabled: boolean) => void;
  // The microphone, under the camera's rules: `microphone = true` is RDP's and
  // a wlshare target's (wlshare's microphone extension) alone, enabled per session, and
  // `micStreaming` is whether the remote records.
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
  // The touchscreen switch. Offered when the host takes touch contacts and
  // this device has a touchscreen; active when it is also on, which is what
  // swaps the Help card's gesture table for the guest's own. See
  // useRemoteDesktop and touchPassthrough.ts.
  touchOffered: boolean;
  touchEnabled: boolean;
  touchActive: boolean;
  onTouchChange: (enabled: boolean) => void;
  // A chord this component took for itself, announced to the input path so it can
  // unwind what it was holding for one. Only the Mac spelling of the chrome
  // shortcut needs it, and only because Command is in it. See useRemoteDesktop.
  onLocalShortcut: () => void;
  // Hands the keyboard back to the remote desktop surface, which is where the key
  // listeners live — they are scoped to the focused surface rather than the window,
  // so a control that keeps focus keeps the keys too. See useRemoteDesktop.
  onFocusDesktop: () => void;
  // Whether this menu currently has something over the desktop, which turns the
  // desktop view-only for as long as it does. The input path is the other side of
  // the page, so this is reported rather than read. See useRemoteDesktop.
  onViewOnlyChange: (viewOnly: boolean) => void;
  // False while the status overlay stands over the desktop: connecting,
  // reconnecting, an error, a claim conflict, or the gap before the first frame.
  desktopShown: boolean;
}) {
  // The one modal card, and which face it shows: Info, or the "Throughput" view its
  // button switches it to.
  const [modal, setModal] = useState<Modal | null>(null);
  const { panel, setPanel, closePanel, togglePanel } = usePanel();
  // True between pressing Clipboard and the remote's text arriving. The panel
  // stays closed for that moment so it never opens on stale text that visibly
  // rewrites itself a beat later.
  const [clipboardPending, setClipboardPending] = useState(false);
  // How much of the bottom edge a docked panel is covering. The canvas already
  // insets above it; the button and its drawer float over the same edge and
  // have to do the same, or the soft keyboard opens on top of the one control
  // that closes it again.
  const [dockedHeight, setDockedHeight] = useState(0);
  const { open, setOpen, hidden, toolbarStyle, button } = useFloatingButton({
    glyph: "☰",
    dockedHeight,
    isMacHost,
    onLocalShortcut,
  });

  // Kept here as well as passed on: one measurement, two readers.
  const onDockedHeight = useCallback(
    (px: number) => {
      setDockedHeight(px);
      onKeyboardInset(px);
    },
    [onKeyboardInset],
  );

  // The toolbar control that opened the modal, which takes focus back when it closes.
  const modalOpenerRef = useRef<HTMLElement | null>(null);
  const closeModal = useCallback(() => {
    setModal(null);
    modalOpenerRef.current?.focus();
  }, []);
  const shown = useMemo(
    () => displaysShown(displays, activeDisplayId),
    [displays, activeDisplayId],
  );
  const backToInfo = useCallback(() => setModal("info"), []);

  // A soft key is input, and the drawer standing over a view-only desktop says
  // input is not happening — so pressing one takes the drawer down with it rather
  // than typing on a remote the label promised was untouched. The panel itself
  // stays: it is closed by its own Close, and it is the half of this the user just
  // said they meant.
  const onSoftKey = useCallback(
    (codes: string[]) => {
      setOpen(false);
      sendKeyCombo(codes);
    },
    [sendKeyCombo, setOpen],
  );

  // Open the on-screen keyboard and collapse the drawer so the panel has the
  // screen to itself; toggling the button again closes the panel.
  const onSoftKeyboard = useCallback(() => {
    togglePanel("keyboard");
    setOpen(false);
  }, [togglePanel, setOpen]);

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
  }, [size, setOpen]);

  // Entering the mode has to hand the keyboard over along with the screen. The button
  // just clicked otherwise keeps focus, and the remote's key listeners sit on the
  // desktop surface rather than the window — so the Super key the lock just won would
  // land on the drawer and reach nothing at all. Closing the drawer by itself drops
  // focus on the body, which is the same silence. Leaving is the same handover in
  // reverse; only a refused request keeps the drawer, which is where it says why.
  const onFullscreenSettled = useCallback(() => {
    onFocusDesktop();
    setOpen(false);
  }, [onFocusDesktop, setOpen]);

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
  }, [
    panel,
    clipboardPending,
    onFetchClipboard,
    closePanel,
    setPanel,
    setOpen,
  ]);

  // The soft keyboard types on the desktop, so it goes when the desktop does,
  // and is not there again when the desktop comes back.
  useEffect(() => {
    if (!desktopShown) {
      setPanel((current) => (current === "keyboard" ? null : current));
    }
  }, [desktopShown, setPanel]);

  const viewOnly = useViewOnly(open, hidden, modal, panel, onViewOnlyChange);
  // A click on the cover takes down what put it there. A modal card has a
  // backdrop of its own over the cover, and the other panels never raise one.
  const dismissCover = useCallback(() => {
    setOpen(false);
    setPanel((current) => (current === "clipboard" ? null : current));
  }, [setPanel, setOpen]);

  return (
    <>
      <ViewOnlyCover over={viewOnly} onDismiss={dismissCover} />

      {/* The button and its drawer go together: a toolbar anchored to a button
          that isn't there reads as a bug. Both keep their state while hidden, so
          the chord brings back exactly what was on screen. Docked panels are left
          alone because they carry their own Close. */}
      {!hidden && <FloatingButton {...button} />}

      {open && !hidden && (
        <div className="toolbar" style={toolbarStyle}>
          <FullscreenSection onSettled={onFullscreenSettled} />

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
            <button
              type="button"
              className="toolbar-btn"
              onClick={onClipboard}
              disabled={clipboardPending}
              aria-pressed={panel === "clipboard"}
              aria-busy={clipboardPending}
              title="Read and write the remote's clipboard"
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
            blocked={audioBlock}
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

          <TouchscreenSection
            offered={touchOffered}
            enabled={touchEnabled}
            onChange={onTouchChange}
          />

          <div className="toolbar-section toolbar-actions">
            <button
              type="button"
              className="toolbar-btn"
              onClick={(e) => {
                modalOpenerRef.current = e.currentTarget;
                setModal("info");
              }}
              title="This session's displays, density, render dial and decoders, with the shortcuts and gestures under Help"
            >
              Info
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
              End session
            </button>
            <button
              type="button"
              className="toolbar-btn toolbar-btn-danger"
              onClick={onLogout}
            >
              Log out
            </button>
          </div>

          <AppVersion className="app-version" />
        </div>
      )}

      <ThroughputModal
        open={modal === "throughput"}
        onBack={backToInfo}
        onDismiss={closeModal}
        onUnauthorized={onUnauthorized}
      />

      {modal === "info" && (
        <ModalOverlay label="Info" className="help-card" onDismiss={closeModal}>
          <h2>Info</h2>
          <ScreenHelp
            size={size}
            hostScale={hostScale}
            connection={connection}
            renderPlan={renderPlan}
            oversize={oversize}
            audio={{
              blocked: audioBlock,
              available: canAudio,
              enabled: audioEnabled,
              error: audioError,
              stream: audioStream,
            }}
            videoStream={videoStream}
          />
          <DisplaysHelp shown={shown} />
          <div className="help-actions">
            <ThroughputButton onOpen={() => setModal("throughput")} />
            <button
              type="button"
              className="toolbar-btn"
              onClick={() => setModal("help")}
            >
              Help
            </button>
            <button type="button" className="toolbar-btn" onClick={closeModal}>
              Close
            </button>
          </div>
          <AppVersion className="app-version" />
        </ModalOverlay>
      )}

      {/* The Info card switched to Help: how the session is driven, apart from
          what it is. Back returns to Info, as from Throughput. */}
      {modal === "help" && (
        <ModalOverlay label="Help" className="help-card" onDismiss={closeModal}>
          <h2>Help</h2>
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
            <ImmersiveHelpRows />
            <AppWindowHelpRow />
          </dl>
          <KeyboardHelp isMacHost={isMacHost} remoteIsMac={remoteIsMac} />
          <h3>Touch gestures</h3>
          {touchActive ? (
            <p className="help-note">
              Touchscreen is on: fingers reach the remote as touch contacts, and
              its own gestures apply — tap, drag, press-and-hold, pinch,
              two-finger scroll, edge swipes. Turn it off for the trackpad
              gestures below.
            </p>
          ) : null}
          <dl className="help-list">
            {GESTURE_HELP.map((row) => (
              <div key={row.gesture} className="help-item">
                <dt>{row.gesture}</dt>
                <dd>{row.action}</dd>
              </div>
            ))}
          </dl>
          <div className="help-actions">
            <button type="button" className="toolbar-btn" onClick={backToInfo}>
              Back to info
            </button>
            <button type="button" className="toolbar-btn" onClick={closeModal}>
              Close
            </button>
          </div>
        </ModalOverlay>
      )}

      <DockedPanel
        panel={panel}
        onClose={closePanel}
        onDockedHeightChange={onDockedHeight}
        sendKeyCombo={onSoftKey}
        onFocusDesktop={onFocusDesktop}
        remoteClipboard={remoteClipboard}
        onSendClipboard={onSendClipboard}
        displays={displays}
        activeDisplayId={activeDisplayId}
        onSelectDisplay={onSelectDisplay}
      />
    </>
  );
}
