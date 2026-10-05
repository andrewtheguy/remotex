import { useCallback, useEffect, useRef } from "react";
import DisplayMenu from "./DisplayMenu.tsx";
import FloatingMenu from "./FloatingMenu.tsx";
import type { DisplayInfo, HoldCause } from "./protocol.ts";
import TargetPicker from "./TargetPicker.tsx";
import {
  CAN_PINCH_ZOOM,
  type ConnectionStatus,
  type RemoteSize,
  useRemoteDesktop,
} from "./useRemoteDesktop.ts";

const STATUS_LABEL: Record<ConnectionStatus, string> = {
  connecting: "Connecting…",
  connected: "Connected",
  reconnecting: "Reconnecting…",
  busy: "Session in use",
  takenOver: "Session taken over",
  failed: "Cannot open the session",
  unavailable: "Display not available",
  idle: "Not connected",
  stale: "Page out of date",
};

// The notice over a desktop with no picture, offering every display but the one
// being sent, which is the one held: past what video carries, or Combined Display over
// more than two screens. A click sends a `selectDisplay` and nothing else: the
// notice comes down when the gateway says the desktop has a picture again.
function OversizeNotice({
  cause,
  size,
  displays,
  activeDisplayId,
  onSelectDisplay,
}: {
  cause: HoldCause;
  size: RemoteSize;
  displays: DisplayInfo[];
  activeDisplayId: number | null;
  onSelectDisplay: (id: number) => void;
}) {
  const others = displays.filter((display) => display.id !== activeDisplayId);
  return (
    <div className="oversize-overlay" role="alert">
      {cause === "screens" ? (
        <>
          <span className="status">Too many screens to show</span>
          <span className="status-hint">
            Combined Display spans more than two screens, which is more than one
            view shows.
          </span>
        </>
      ) : (
        <>
          <span className="status">Too large to show</span>
          <span className="status-hint">
            The remote desktop is {size.w}×{size.h} pixels, past the largest
            picture a video stream carries.
          </span>
        </>
      )}
      {others.length > 0 ? (
        <>
          <span className="status-hint">
            Choose one display to show on its own:
          </span>
          <div className="oversize-displays">
            {others.map((display) => (
              <button
                type="button"
                key={display.id}
                className="status-action"
                onClick={() => onSelectDisplay(display.id)}
              >
                {display.label}
              </button>
            ))}
          </div>
        </>
      ) : (
        <span className="status-hint">
          Nothing can be shown until the remote's desktop is smaller.
        </span>
      )}
    </div>
  );
}

// What can lie over a live session's desktop, under the menu, which stays the way
// to another target from all three.
function SessionCovers({
  resizing,
  oversize,
  size,
  displays,
  activeDisplayId,
  onSelectDisplay,
}: {
  resizing: boolean;
  oversize: HoldCause | null;
  size: RemoteSize | null;
  displays: DisplayInfo[];
  activeDisplayId: number | null;
  onSelectDisplay: (id: number) => void;
}) {
  return (
    <>
      {/* A High Performance resize that has not settled: the Mac's intermediate
          modes and repaints stay behind this, as they do behind Apple's own
          client's. It takes no input and sits below the menu, so the menu stays
          reachable; the gateway says when it comes down. */}
      {resizing && (
        <output className="resize-overlay">
          <span className="status">Resizing…</span>
        </output>
      )}

      {/* A desktop past what video carries: no picture comes, and the session stays
          up for the one way out, a smaller desktop from the remote. Choosing one of
          its displays is that way for a Mac on Combined Display, so they are offered
          here and not only in the menu. It takes the pointer, since the remote
          under it is not on screen. */}
      {oversize && size && (
        <OversizeNotice
          cause={oversize}
          size={size}
          displays={displays}
          activeDisplayId={activeDisplayId}
          onSelectDisplay={onSelectDisplay}
        />
      )}
    </>
  );
}

// What stands over the page while there is no desktop to show: the connection's
// lifecycle, a claim conflict, or the gap before the first frame.
function StatusOverlay({
  branding,
  status,
  connectError,
  waiting,
  tabDisplay,
  onTakeOver,
  onRetry,
}: {
  branding: string;
  status: ConnectionStatus;
  connectError: string | null;
  tabDisplay: number | null;
  // The session is up and its first frame has not come.
  waiting: boolean;
  onTakeOver: () => void;
  onRetry: () => void;
}) {
  return (
    <div className="status-overlay">
      <span className="status-brand">{branding}</span>
      <span className={`status status-${status}`}>
        {tabDisplay !== null && status === "takenOver"
          ? "Display taken over"
          : tabDisplay !== null && status === "busy"
            ? "Display in use"
            : STATUS_LABEL[status]}
      </span>
      {/* Why the session is not up, when the reason is known. "Reconnecting…"
          is true and unhelpful next to "the server answered 502", and the
          picker is not on screen to carry it while the overlay is. */}
      {connectError && <span className="status-hint">{connectError}</span>}
      {waiting && (
        <span className="status-hint">Waiting for the remote desktop…</span>
      )}
      {status === "busy" && (
        <>
          <span className="status-hint">
            {tabDisplay === null
              ? "This desktop is open in another browser."
              : `Display ${tabDisplay} is open in another tab, and is shown in one tab at a time.`}
          </span>
          <button type="button" className="status-action" onClick={onTakeOver}>
            Take over
          </button>
        </>
      )}
      {(status === "failed" || status === "unavailable") && (
        <button type="button" className="status-action" onClick={onRetry}>
          Retry
        </button>
      )}
      {/* A display's tab that was disconnected from its menu: one tab shows the
          display at a time, and the one that connects takes it. */}
      {status === "idle" && (
        <>
          <span className="status-hint">
            Display {tabDisplay} is not shown in this tab. Connect to show it
            here.
          </span>
          <button type="button" className="status-action" onClick={onRetry}>
            Connect
          </button>
        </>
      )}
      {status === "stale" && (
        <button
          type="button"
          className="status-action"
          onClick={() => location.reload()}
        >
          Reload
        </button>
      )}
      {status === "takenOver" && (
        <>
          <span className="status-hint">
            {tabDisplay === null
              ? "Another browser took over this session."
              : `Another tab took over display ${tabDisplay}.`}
          </span>
          <button type="button" className="status-action" onClick={onTakeOver}>
            Take it back
          </button>
        </>
      )}
    </div>
  );
}

export default function RemoteDesktop({
  branding,
  tabDisplay,
  onLogout,
  onUnauthorized,
}: {
  /** Deployment display name shown on the interstitials. */
  branding: string;
  /** The display this page shows in a tab of its own (`/display/N`), beside the
   *  session another tab of this browser holds: its picture and input, a menu
   *  of its own for what is this tab's, and no picker. Null on the page that
   *  holds the session. */
  tabDisplay: number | null;
  onLogout: () => void;
  onUnauthorized: () => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const graphicsRef = useRef<HTMLCanvasElement>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const pointerRef = useRef<HTMLImageElement>(null);
  // The keyboard belongs to the overlay, whose key listeners are scoped to it
  // rather than the window; the menu calls this when a control of its own has
  // taken focus and is done with it. See FloatingMenu and useRemoteDesktop.
  const focusDesktop = useCallback(
    () => overlayRef.current?.focus({ preventScroll: true }),
    [],
  );
  const {
    status,
    mode,
    connectError,
    pendingTarget,
    size,
    hostScale,
    renderPlan,
    oversize,
    connection,
    canAudio,
    audioEnabled,
    audioError,
    videoError,
    audioStream,
    videoStream,
    canCamera,
    cameraEnabled,
    cameraError,
    cameraStreaming,
    canMic,
    micEnabled,
    micError,
    micStreaming,
    displays,
    activeDisplayId,
    remoteClipboard,
    macKeyOverridesEnabled,
    macKeyOverridesActive,
    isMacHost,
    remoteIsMac,
    setMacKeyOverridesEnabled,
    touchOffered,
    touchEnabled,
    touchActive,
    setTouchEnabled,
    remoteResizing,
    setViewOnly,
    onLocalShortcut,
    takeOver,
    retry,
    releaseTab,
    connect,
    switchTarget,
    selectDisplay,
    setAudio,
    setCamera,
    setMic,
    sendKeyCombo,
    requestClipboard,
    sendClipboard,
    setBottomInset,
  } = useRemoteDesktop(
    canvasRef,
    graphicsRef,
    overlayRef,
    pointerRef,
    onUnauthorized,
    tabDisplay,
  );

  // A speaker on the tab title while sound is playing, and a camera and a
  // microphone while each is offered — the one place the desktop has room to
  // say so, since the toggles live in the drawer, and for the camera and the
  // microphone it is also the honest little recording light. At the *front*,
  // not the end: a tab title is truncated from the right, so a suffix is the
  // first thing to vanish. Desktop only, so the picker's tab stays the plain
  // branding.
  useEffect(() => {
    const marks =
      mode === "desktop"
        ? `${cameraEnabled ? "🎥 " : ""}${micEnabled ? "🎤 " : ""}${audioEnabled ? "🔊 " : ""}`
        : "";
    const shown = tabDisplay === null ? "" : `Display ${tabDisplay} · `;
    document.title = `${marks}${shown}${branding}`;
  }, [mode, audioEnabled, cameraEnabled, micEnabled, branding, tabDisplay]);

  // The status overlay covers the connection lifecycle (connecting/reconnecting)
  // and the claim conflicts (busy/takenOver); in the desktop it also covers the
  // gap before the first frame. The picker owns the screen once connected.
  const showStatus = status !== "connected" || (mode === "desktop" && !size);

  return (
    /* screen-touch swaps native scrolling for the gesture transform
       (pinch zoom + pan) and stretches the input overlay over the whole
       viewport so gestures land everywhere — see index.css. */
    <div className={`screen${CAN_PINCH_ZOOM ? " screen-touch" : ""}`}>
      <div className="surface">
        {/* Starts 0×0 so no ghost block shows before the first resize; the
            resize handler keeps the full pixel bitmap separate from its
            remote-point CSS size. Kept
            mounted in both modes so the hook's canvas ref stays stable. */}
        <canvas ref={canvasRef} className="framebuffer" width={0} height={0} />
        {/* What is drawn on the GPU is drawn here and not on the canvas above:
            an RDP host's graphics pipeline, passed through, and the software
            HEVC decoder's pictures (glPicture.ts). The paint worker shows it
            while it holds one of them. It takes the canvas above's box, and
            lies under the input overlay like it. */}
        <canvas
          ref={graphicsRef}
          className="framebuffer graphics"
          width={0}
          height={0}
        />
        {/* Transparent overlay captures mouse + keyboard input. tabIndex
            makes the div focusable — without it, focus() in the mousedown
            handler is a no-op and the keydown/keyup listeners (scoped to
            the focused overlay, not window) never fire. */}
        <div
          ref={overlayRef}
          className="input-overlay"
          role="application"
          // biome-ignore lint/a11y/noNoninteractiveTabindex: the remote-desktop surface (role=application) must take focus to receive keyboard input
          tabIndex={0}
        />
        {/* The pointer for the touch gesture layer's virtual cursor, drawn
            only when the engine sends cursor shapes instead of compositing
            them (VNC). Sized and positioned imperatively by the hook; hidden
            by default, and decorative, so it carries no alt text. */}
        <img ref={pointerRef} className="remote-pointer" alt="" />
      </div>

      {/* The floating menu is desktop-only; its End session button returns to
          the picker (see FloatingMenu.tsx), and Log out ends the login. A second
          display's tab has a menu of its own instead, for this tab's full screen. */}
      {mode === "desktop" &&
        (tabDisplay !== null ? (
          <DisplayMenu
            display={tabDisplay}
            connected={status === "connected"}
            size={size}
            isMacHost={isMacHost}
            onLocalShortcut={onLocalShortcut}
            onFocusDesktop={focusDesktop}
            onViewOnlyChange={setViewOnly}
            onDisconnect={releaseTab}
          />
        ) : (
          <FloatingMenu
            onLogout={onLogout}
            onUnauthorized={onUnauthorized}
            onSwitchTarget={switchTarget}
            sendKeyCombo={sendKeyCombo}
            onKeyboardInset={setBottomInset}
            remoteClipboard={remoteClipboard}
            onFetchClipboard={requestClipboard}
            onSendClipboard={sendClipboard}
            displays={displays}
            activeDisplayId={activeDisplayId}
            onSelectDisplay={selectDisplay}
            size={size}
            hostScale={hostScale}
            connection={connection}
            renderPlan={renderPlan}
            oversize={oversize}
            canAudio={canAudio}
            audioEnabled={audioEnabled}
            audioError={audioError}
            audioStream={audioStream}
            videoStream={videoStream}
            onAudioChange={setAudio}
            canCamera={canCamera}
            cameraEnabled={cameraEnabled}
            cameraError={cameraError}
            cameraStreaming={cameraStreaming}
            onCameraChange={setCamera}
            canMic={canMic}
            micEnabled={micEnabled}
            micError={micError}
            micStreaming={micStreaming}
            onMicChange={setMic}
            macKeyOverridesEnabled={macKeyOverridesEnabled}
            macKeyOverridesActive={macKeyOverridesActive}
            isMacHost={isMacHost}
            remoteIsMac={remoteIsMac}
            onMacKeyOverridesChange={setMacKeyOverridesEnabled}
            touchOffered={touchOffered}
            touchEnabled={touchEnabled}
            touchActive={touchActive}
            onTouchChange={setTouchEnabled}
            onLocalShortcut={onLocalShortcut}
            onFocusDesktop={focusDesktop}
            onViewOnlyChange={setViewOnly}
            desktopShown={!showStatus}
          />
        ))}

      {/* The post-login target picker: shown once the slot is held and no
          target is connected. */}
      {status === "connected" && mode === "picker" && tabDisplay === null && (
        <TargetPicker
          branding={branding}
          connect={connect}
          pendingTarget={pendingTarget}
          connectError={connectError}
          onLogout={onLogout}
          onUnauthorized={onUnauthorized}
        />
      )}

      {/* A video target this browser cannot decode. Its own banner rather than a
          line in the status overlay, because the overlay hides itself the moment the
          session is up — and this is a session that *is* up, showing nothing. It is
          also not `connectError`: nothing is wrong with the session or the gateway,
          it is this browser that cannot decode what is arriving. */}
      {videoError && mode === "desktop" && (
        <div className="video-banner" role="alert">
          {videoError}
        </div>
      )}

      {mode === "desktop" && !showStatus && (
        <SessionCovers
          resizing={remoteResizing}
          oversize={oversize}
          size={size}
          displays={displays}
          activeDisplayId={activeDisplayId}
          onSelectDisplay={selectDisplay}
        />
      )}

      {showStatus && (
        <StatusOverlay
          branding={branding}
          status={status}
          connectError={connectError}
          waiting={status === "connected"}
          tabDisplay={tabDisplay}
          onTakeOver={takeOver}
          onRetry={retry}
        />
      )}
    </div>
  );
}
