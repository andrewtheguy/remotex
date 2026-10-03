import { useCallback, useEffect } from "react";
import { FullscreenSection, ViewOnlyCover } from "./FloatingMenu.tsx";
import { FloatingButton, useFloatingButton } from "./floatingButton.tsx";

// The menu on a second display's tab: the session page's button and drawer
// (floatingButton.tsx), holding what belongs to this tab alone. That is immersive
// full screen and nothing else so far — full screen is a window's, and the session
// page's button reaches only its own. Everything the session has one of, its sound,
// clipboard, display picker and End session, stays on the session's page.
//
// The button shows the display's number where the session's shows ☰, so the two
// windows of one session are told apart by the control each carries.
export default function DisplayMenu({
  display,
  isMacHost,
  onLocalShortcut,
  onFocusDesktop,
  onViewOnlyChange,
}: {
  display: number;
  isMacHost: boolean;
  // The three FloatingMenu takes, for the same reasons: the chord that hides the
  // button, the keyboard going back to the desktop surface, and the desktop being
  // view-only while the drawer is over it.
  onLocalShortcut: () => void;
  onFocusDesktop: () => void;
  onViewOnlyChange: (viewOnly: boolean) => void;
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

  return (
    <>
      <ViewOnlyCover over={drawerUp ? "menu" : null} onDismiss={close} />
      {!hidden && <FloatingButton {...button} />}
      {drawerUp && (
        <div className="toolbar" style={toolbarStyle}>
          <FullscreenSection onSettled={onFullscreenSettled} />
        </div>
      )}
    </>
  );
}
