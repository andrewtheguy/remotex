import { useRef } from "react";
import { useDockedHeight } from "./SoftKeyboardPanel.tsx";

// The remote's resolution menu, as a docked panel rather than a row inside the
// floating menu's drawer.
//
// A list, not a "resize to window" button, because the only remote that offers
// one is a Mac agent on a virtual display, and such a display takes nothing but
// the sizes it advertises — see ClientMsg::SetResolution in src/protocol.rs.
// The list is regenerated on the Mac whenever its display is reconfigured, so
// it can change under an open panel; that is fine, it is rendered from props.
//
// The gateway applies the nearest usable mode when a size is no longer offered,
// so a stale pick lands somewhere sensible instead of failing.

interface ResolutionPanelProps {
  modes: { w: number; h: number }[];
  // The remote's current size, marked in the list. Null before the first
  // `resize` arrives.
  current: { w: number; h: number } | null;
  onPick: (w: number, h: number) => void;
  onClose: () => void;
  // Same docked-height contract as the clipboard and soft-keyboard panels: the
  // touch canvas insets above whichever one is open.
  onDockedHeightChange?: (px: number) => void;
}

export function ResolutionPanel({
  modes,
  current,
  onPick,
  onClose,
  onDockedHeightChange,
}: ResolutionPanelProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  useDockedHeight(panelRef, onDockedHeightChange);

  return (
    <div className="panel" ref={panelRef}>
      <div className="panel-header">
        <span className="panel-title">Resolution</span>
        <button
          type="button"
          className="panel-close"
          aria-label="Close resolution"
          onClick={onClose}
        >
          ✕
        </button>
      </div>

      <div className="res-modes">
        {modes.length === 0 ? (
          <span>No display modes are currently available.</span>
        ) : (
          modes.map((mode) => {
            const active = current?.w === mode.w && current?.h === mode.h;
            return (
              <button
                key={`${mode.w}x${mode.h}`}
                type="button"
                className={active ? "res-mode res-mode-active" : "res-mode"}
                onClick={() => onPick(mode.w, mode.h)}
                aria-pressed={active}
                title={
                  active
                    ? `The remote is already ${mode.w}×${mode.h}`
                    : `Set the remote display to ${mode.w}×${mode.h}`
                }
              >
                {mode.w} × {mode.h}
              </button>
            );
          })
        )}
      </div>
    </div>
  );
}
