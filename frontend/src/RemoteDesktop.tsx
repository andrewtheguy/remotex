import { useRef } from "react";
import { useRemoteDesktop } from "./useRemoteDesktop.ts";

const STATUS_LABEL: Record<string, string> = {
  connecting: "Connecting…",
  connected: "Connected (skeleton — no RDP backend yet)",
  closed: "Disconnected",
  error: "Connection error",
};

export default function RemoteDesktop() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const { status } = useRemoteDesktop(overlayRef);

  return (
    <div className="remote-desktop">
      <div className="statusbar">
        <span className={`status status-${status}`}>{STATUS_LABEL[status]}</span>
      </div>
      <div className="screen">
        {/* TODO(phase1): render decoded tiles onto this canvas. */}
        <canvas ref={canvasRef} className="framebuffer" width={1024} height={768} />
        {/* Transparent overlay captures mouse + keyboard input. */}
        <div ref={overlayRef} className="input-overlay" tabIndex={0}>
          <div className="placeholder">
            No remote screen yet — RDP engine lands in Phase 1.
            <br />
            Mouse and keyboard events over this area are sent to the backend.
          </div>
        </div>
      </div>
    </div>
  );
}
