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
// Standard Apple Screen Sharing (`subtype = "ard"`) sends the Mac's physical
// screens plus an "Combined Display" entry. High Performance mode sends one virtual
// display, leaving nothing to choose. A wlshare desktop sends the compositor's
// outputs, one of which it is capturing. An RDP target or a High Performance Mac
// asked for virtual displays (alpha) sends the ones the host laid out, and shows
// one of them — or, on its All Displays, the first here and each other one in a
// browser tab of its own, which this panel links to; every other RDP or VNC
// session exposes one framebuffer and no list.

import { type ReactNode, useRef } from "react";
import { useDockedHeight, useIsDesktop } from "./dockedPanel.ts";
import { gatewayUrl } from "./gateway.ts";
import type { DisplayInfo } from "./protocol.ts";

interface Props {
  displays: DisplayInfo[];
  activeId: number | null;
  onSelect: (id: number) => void;
  onClose: () => void;
  onDockedHeightChange: (height: number) => void;
}

// The link to a display shown in a tab of its own. It opens in this browser,
// which is what lets its page in: it carries the login cookie, and is given no
// session token. `noopener`, so the new tab shares nothing of this one's page
// state — the session token in this tab's storage included.
export function DisplayTabLink({
  tab,
  className = "dp-tab-link",
  title,
  children,
}: {
  tab: number;
  className?: string;
  title?: string;
  children: ReactNode;
}) {
  return (
    <a
      className={className}
      href={gatewayUrl(`/display/${tab}`)}
      target="_blank"
      rel="noopener"
      title={title}
    >
      {children}
    </a>
  );
}

export default function DisplayPanel({
  displays,
  activeId,
  onSelect,
  onClose,
  onDockedHeightChange,
}: Props) {
  const panelRef = useRef<HTMLDivElement | null>(null);
  useDockedHeight(panelRef, !useIsDesktop(), onDockedHeightChange);
  const tabs = displays.filter((display) => display.tab !== null);

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

      {tabs.length > 0 && (
        <div className="dp-tabs">
          {tabs.map(
            (display) =>
              display.tab !== null && (
                <DisplayTabLink key={display.id} tab={display.tab}>
                  Open {display.label} in a new tab ↗
                </DisplayTabLink>
              ),
          )}
        </div>
      )}
    </div>
  );
}
