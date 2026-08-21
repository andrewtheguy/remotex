import { useEffect, useRef } from "react";
import FloatingMenu from "./FloatingMenu.tsx";
import TargetPicker from "./TargetPicker.tsx";
import {
  CAN_PINCH_ZOOM,
  type ConnectionStatus,
  useRemoteDesktop,
} from "./useRemoteDesktop.ts";

const STATUS_LABEL: Record<ConnectionStatus, string> = {
  connecting: "Connecting…",
  connected: "Connected",
  reconnecting: "Reconnecting…",
  busy: "Session in use",
  takenOver: "Session taken over",
  failed: "Cannot open the session",
};

export default function RemoteDesktop({
  branding,
  onLogout,
  onUnauthorized,
}: {
  /** Deployment display name shown on the interstitials. */
  branding: string;
  onLogout: () => void;
  onUnauthorized: () => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const pointerRef = useRef<HTMLImageElement>(null);
  const {
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
    canCamera,
    cameraEnabled,
    cameraError,
    cameraStreaming,
    canMic,
    micEnabled,
    micError,
    micStreaming,
    videoError,
    audioStream,
    videoStreams,
    audioByDefault,
    setAudioByDefault,
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
    onLocalShortcut,
    takeOver,
    retry,
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
  } = useRemoteDesktop(canvasRef, overlayRef, pointerRef, onUnauthorized);

  // A speaker on the tab title while sound is playing, a camera and a microphone
  // while each is offered — the one place the desktop has room to say so, since
  // the toggles live in the drawer, and for the camera and mic it is also the
  // honest little recording light. At the *front*, not the end: a tab title is
  // truncated from the right, so a suffix is the first thing to vanish. Desktop
  // only, so the picker's tab stays the plain branding.
  useEffect(() => {
    const marks =
      mode === "desktop"
        ? `${cameraEnabled ? "🎥 " : ""}${micEnabled ? "🎤 " : ""}${audioEnabled ? "🔊 " : ""}`
        : "";
    document.title = `${marks}${branding}`;
  }, [mode, audioEnabled, cameraEnabled, micEnabled, branding]);

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

      {/* The floating menu is desktop-only; its Switch target button returns to
          the picker (see FloatingMenu.tsx), and Log out ends the login. */}
      {mode === "desktop" && (
        <FloatingMenu
          onLogout={onLogout}
          onSwitchTarget={switchTarget}
          sendKeyCombo={sendKeyCombo}
          onKeyboardInset={setBottomInset}
          canClipboard={canClipboard}
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
          canAudio={canAudio}
          audioEnabled={audioEnabled}
          audioError={audioError}
          audioStream={audioStream}
          videoStreams={videoStreams}
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
        />
      )}

      {/* The post-login target picker: shown once the slot is held and no
          target is connected. */}
      {status === "connected" && mode === "picker" && (
        <TargetPicker
          branding={branding}
          connect={connect}
          pendingTarget={pendingTarget}
          connectError={connectError}
          audioByDefault={audioByDefault}
          onAudioByDefaultChange={setAudioByDefault}
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

      {showStatus && (
        <div className="status-overlay">
          <span className="status-brand">{branding}</span>
          <span className={`status status-${status}`}>
            {STATUS_LABEL[status]}
          </span>
          {/* Why the session is not up, when the reason is known. "Reconnecting…"
              is true and unhelpful next to "the server answered 502", and the
              picker is not on screen to carry it while the overlay is. */}
          {connectError && <span className="status-hint">{connectError}</span>}
          {status === "connected" && mode === "desktop" && !size && (
            <span className="status-hint">Waiting for the remote desktop…</span>
          )}
          {status === "busy" && (
            <>
              <span className="status-hint">
                This desktop is open in another browser.
              </span>
              <button
                type="button"
                className="status-action"
                onClick={takeOver}
              >
                Take over
              </button>
            </>
          )}
          {status === "failed" && (
            <button type="button" className="status-action" onClick={retry}>
              Retry
            </button>
          )}
          {status === "takenOver" && (
            <>
              <span className="status-hint">
                Another browser took over this session.
              </span>
              <button
                type="button"
                className="status-action"
                onClick={takeOver}
              >
                Take it back
              </button>
            </>
          )}
        </div>
      )}
    </div>
  );
}
