import { useCallback, useEffect } from "react";
import { FullscreenSection, ViewOnlyCover } from "./FloatingMenu.tsx";
import { FloatingButton, useFloatingButton } from "./floatingButton.tsx";

// The menu on a second display's tab: the session page's button and drawer
// (floatingButton.tsx), holding what belongs to this tab alone. That is immersive
// full screen — full screen is a window's, and the session page's button reaches
// only its own — and Disconnect, which gives the display up for another tab to
// take. Everything the session has one of, its sound, clipboard, display picker
// and End session, stays on the session's page.
//
// The button shows the display's number where the session's shows ☰, so the two
// windows of one session are told apart by the control each carries.
export default function DisplayMenu({
  display,
  connected,
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
  isMacHost: boolean;
  // The three FloatingMenu takes, for the same reasons: the chord that hides the
  // button, the keyboard going back to the desktop surface, and the desktop being
  // view-only while the drawer is over it.
  onLocalShortcut: () => void;
  onFocusDesktop: () => void;
  onViewOnlyChange: (viewOnly: boolean) => void;
  // Give this display up: the tab goes back to asking, and the display is the
  // next tab's to connect to. See useRemoteDesktop.
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
            <button
              type="button"
              className="toolbar-btn"
              onClick={() => {
                setOpen(false);
                onDisconnect();
              }}
              title="Stop showing this display here, so another tab can connect to it"
            >
              Disconnect
            </button>
          </div>
        </div>
      )}
    </>
  );
}
