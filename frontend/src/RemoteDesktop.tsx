import { useRef } from "react";
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
  error: "Session error",
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
  const { status, size, errorMessage, takeOver, retry } = useRemoteDesktop(
    canvasRef,
    overlayRef,
    onLogout,
    onUnauthorized,
  );

  return (
    /* screen-touch swaps native scrolling for the phase-8 gesture transform
       (pinch zoom + pan) and stretches the input overlay over the whole
       viewport so gestures land everywhere — see index.css. */
    <div className={`screen${CAN_PINCH_ZOOM ? " screen-touch" : ""}`}>
      <div className="surface">
        {/* Starts 0×0 so no ghost block shows before the first resize; the
            resize handler sets the pixel size and the 1:1 CSS size. */}
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
      {/* Minimal disconnect CTA in the dead space below the fixed-size canvas
          (mobile portrait always has some, since the desktop keeps its
          configured size). Disconnecting logs this browser out — same as the
          Ctrl+Alt+Shift+L chord. CSS shows it on touch devices only; desktop
          uses the chord. */}
      <div className="disconnect-bar">
        <button type="button" className="disconnect-button" onClick={onLogout}>
          Disconnect
        </button>
      </div>
      {(status !== "connected" || !size) && (
        <div className="status-overlay">
          <span className={`status status-${status}`}>
            {STATUS_LABEL[status]}
          </span>
          {status === "connected" && !size && (
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
          {status === "error" && (
            <>
              {errorMessage && (
                <span className="status-hint">{errorMessage}</span>
              )}
              <button type="button" className="status-action" onClick={retry}>
                Retry
              </button>
            </>
          )}
        </div>
      )}
    </div>
  );
}
