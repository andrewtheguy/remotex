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
    <div className="screen">
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
      {(status !== "connected" || !size) && (
        <div className="status-overlay">
          <span className={`status status-${status}`}>
            {STATUS_LABEL[status]}
          </span>
          {status === "connected" && !size && (
            <span className="status-hint">Waiting for the remote desktop…</span>
          )}
        </div>
      )}
    </div>
  );
}
