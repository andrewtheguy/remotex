import { useRef } from "react";
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
};

export default function RemoteDesktop({
  onLogout,
  onUnauthorized,
}: {
  onLogout: () => void;
  onUnauthorized: () => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const {
    status,
    mode,
    connectError,
    pendingTarget,
    size,
    takeOver,
    connect,
    switchTarget,
    sendKeyCombo,
    setBottomInset,
  } = useRemoteDesktop(canvasRef, overlayRef, onUnauthorized);

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
            resize handler sets the pixel size and the 1:1 CSS size. Kept
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
      </div>

      {/* The floating menu is desktop-only; its Switch target button returns to
          the picker (see FloatingMenu.tsx), and Log out ends the login. */}
      {mode === "desktop" && (
        <FloatingMenu
          onLogout={onLogout}
          onSwitchTarget={switchTarget}
          sendKeyCombo={sendKeyCombo}
          onKeyboardInset={setBottomInset}
        />
      )}

      {/* The post-login target picker: shown once the slot is held and no
          target is connected. */}
      {status === "connected" && mode === "picker" && (
        <TargetPicker
          connect={connect}
          pendingTarget={pendingTarget}
          connectError={connectError}
          onLogout={onLogout}
          onUnauthorized={onUnauthorized}
        />
      )}

      {showStatus && (
        <div className="status-overlay">
          <span className={`status status-${status}`}>
            {STATUS_LABEL[status]}
          </span>
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
