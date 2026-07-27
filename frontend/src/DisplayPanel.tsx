// Which of the remote's displays to look at.
//
// The counterpart to the clipboard panel in chrome and in nothing else: there
// is no state here at all. The list and the checkmark come from the server's
// `displays` control message, a click sends a `selectDisplay`, and the mark
// moves only when the remote reports that it moved. So a selection the remote
// refused — an unplugged screen, a capture that would not start — leaves this
// panel agreeing with what is actually on the canvas rather than with what was
// clicked.
//
// Only `rxa` populates the list. RDP and VNC each deliver one framebuffer
// spanning every remote screen, so they send nothing and the FAB shows no
// Display section at all.

import { useRef } from "react";
import type { DisplayInfo } from "./protocol.ts";
import { useDockedHeight } from "./SoftKeyboardPanel.tsx";

interface Props {
  displays: DisplayInfo[];
  activeId: number | null;
  onSelect: (id: number) => void;
  onClose: () => void;
  onDockedHeightChange: (height: number) => void;
}

export default function DisplayPanel({
  displays,
  activeId,
  onSelect,
  onClose,
  onDockedHeightChange,
}: Props) {
  const panelRef = useRef<HTMLDivElement | null>(null);
  useDockedHeight(panelRef, onDockedHeightChange);

  return (
    <div className="panel" ref={panelRef}>
      <div className="panel-header">
        <span className="panel-title">Display</span>
        <button
          type="button"
          className="panel-close"
          aria-label="Close display picker"
          onClick={onClose}
        >
          ✕
        </button>
      </div>

      {/* `aria-pressed` rather than a radio group: these are buttons that act
          on the remote, not a form control whose value is read back on submit,
          and exactly one is pressed at a time. */}
      <div className="dp-list">
        {displays.map((display) => {
          const active = display.id === activeId;
          return (
            <button
              type="button"
              key={display.id}
              aria-pressed={active}
              className={active ? "dp-item dp-item-active" : "dp-item"}
              onClick={() => {
                // Closing immediately, before the remote has answered: the
                // switch takes a capture restart and a repaint, and holding the
                // panel over the canvas for it would hide the one thing worth
                // watching. The checkmark is right the next time it is opened.
                onSelect(display.id);
                onClose();
              }}
            >
              <span className="dp-check" aria-hidden="true">
                {active ? "✓" : ""}
              </span>
              <span className="dp-text">
                <span className="dp-label">{display.label}</span>
                <span className="dp-detail">{display.detail}</span>
              </span>
            </button>
          );
        })}
      </div>
    </div>
  );
}
