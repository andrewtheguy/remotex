import { useCallback, useEffect } from "react";
import {
  FullscreenSection,
  remoteSizeLabel,
  ViewOnlyCover,
} from "./FloatingMenu.tsx";
import { FloatingButton, useFloatingButton } from "./floatingButton.tsx";
import type { RemoteSize } from "./useRemoteDesktop.ts";

// The menu on a second display's tab: the session page's button and drawer
// (floatingButton.tsx), holding what belongs to this tab alone. That is immersive
// full screen — full screen is a window's, and the session page's button reaches
// only its own — and Disconnect, which stops showing the display in this tab.
// Everything the session has one of, its sound, clipboard, display picker
// and End session, stays on the session's page.
//
// The button shows the display's number where the session's shows ☰, so the two
// windows of one session are told apart by the control each carries.
export default function DisplayMenu({
  display,
  connected,
  size,
  isMacHost,
  onLocalShortcut,
  onFocusDesktop,
  onViewOnlyChange,
  onDisconnect,
}: {
  display: number;
  // Whether the display is on screen here. The menu is the display's, so there
  // is none over the page that asks to connect or says why it cannot.
  connected: boolean;
  // What this display is drawn at, which arrives on this tab's socket and nowhere
  // else: the session page's Info has its own display's and not this one's.
  size: RemoteSize | null;
  isMacHost: boolean;
  // The three FloatingMenu takes, for the same reasons: the chord that hides the
  // button, the keyboard going back to the desktop surface, and the desktop being
  // view-only while the drawer is over it.
  onLocalShortcut: () => void;
  onFocusDesktop: () => void;
  onViewOnlyChange: (viewOnly: boolean) => void;
  // Stop showing this display here: the tab closes its socket and waits to be
  // asked to connect again. See useRemoteDesktop.
  onDisconnect: () => void;
}) {
  const { open, setOpen, hidden, toolbarStyle, button } = useFloatingButton({
    glyph: String(display),
    dockedHeight: 0,
    isMacHost,
    onLocalShortcut,
  });

  const drawerUp = open && !hidden;
  useEffect(() => {
    if (!drawerUp) {
      return;
    }
    onViewOnlyChange(true);
    return () => onViewOnlyChange(false);
  }, [drawerUp, onViewOnlyChange]);

  const close = useCallback(() => setOpen(false), [setOpen]);
  // The keyboard goes to the desktop with the screen, as on the session's page.
  const onFullscreenSettled = useCallback(() => {
    onFocusDesktop();
    setOpen(false);
  }, [onFocusDesktop, setOpen]);

  if (!connected) {
    return null;
  }
  return (
    <>
      <ViewOnlyCover over={drawerUp ? "menu" : null} onDismiss={close} />
      {!hidden && <FloatingButton {...button} />}
      {drawerUp && (
        <div className="toolbar" style={toolbarStyle}>
          <FullscreenSection onSettled={onFullscreenSettled} />
          <div className="toolbar-section">
            <span className="toolbar-label">Display {display}</span>
            {size && <p className="toolbar-note">{remoteSizeLabel(size)}</p>}
            <button
              type="button"
              className="toolbar-btn"
              onClick={() => {
                setOpen(false);
                onDisconnect();
              }}
              title="Stop showing this display in this tab"
            >
              Disconnect
            </button>
          </div>
        </div>
      )}
    </>
  );
}
