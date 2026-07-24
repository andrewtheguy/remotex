import { useRef } from "react";
import { useRemoteDesktop } from "./useRemoteDesktop.ts";

const STATUS_LABEL: Record<string, string> = {
  connecting: "Connecting…",
  connected: "Connected",
  closed: "Disconnected",
  error: "Error",
};

export default function RemoteDesktop() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const { status, size } = useRemoteDesktop(canvasRef, overlayRef);

  return (
    <div className="remote-desktop">
      <div className="statusbar">
        <span className={`status status-${status}`}>
          {STATUS_LABEL[status]}
        </span>
        {size && (
          <span className="resolution">
            {size.w}×{size.h}
          </span>
        )}
      </div>
      <div className="screen">
        <div className="surface">
          <canvas
            ref={canvasRef}
            className="framebuffer"
            width={1024}
            height={768}
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
          >
            {!size && (
              <div className="placeholder">
                Waiting for the remote desktop…
                <br />
                Mouse and keyboard over this area drive the remote session.
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
