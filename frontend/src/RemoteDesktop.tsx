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
          {/* Transparent overlay captures mouse + keyboard input. */}
          <div ref={overlayRef} className="input-overlay">
            {!size && (
              <div className="placeholder">
                Waiting for the remote desktop…
                <br />
                Mouse and keyboard over this area drive the RDP session.
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
