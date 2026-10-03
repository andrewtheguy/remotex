// What the bottom-docked panels share: the breakpoint that decides docked
// against floating for the clipboard and display panels, and the one channel
// through which whichever panel is docked reports its height, so the touch
// canvas can pan up above it instead of hiding under it.
import { type RefObject, useEffect, useState } from "react";

// Wide viewports float a panel as a card; narrow ones dock it along the bottom
// edge. The soft keyboard decides by device instead (see SoftKeyboardPanel).
export function useIsDesktop(breakpoint = 800): boolean {
  const [desktop, setDesktop] = useState(() => window.innerWidth >= breakpoint);
  useEffect(() => {
    const mql = window.matchMedia(`(min-width: ${breakpoint}px)`);
    const handler = (e: MediaQueryListEvent) => setDesktop(e.matches);
    mql.addEventListener("change", handler);
    return () => mql.removeEventListener("change", handler);
  }, [breakpoint]);
  return desktop;
}

// Report a bottom-docked panel's height, and 0 whenever it isn't covering
// anything — floating, or unmounted. Every panel that docks to the bottom edge
// shares one inset channel, so they have to agree on this exactly; keeping it
// in one place is what makes "only one panel is ever open" safe to rely on.
// `docked` is the panel's own call: the panel knows why it docks.
export function useDockedHeight(
  panelRef: RefObject<HTMLDivElement | null>,
  docked: boolean,
  onDockedHeightChange: ((px: number) => void) | undefined,
) {
  useEffect(() => {
    const report = onDockedHeightChange;
    if (!report) {
      return;
    }
    if (!docked) {
      report(0);
      return;
    }
    const panel = panelRef.current;
    if (!panel) {
      return;
    }
    const measure = () => report(panel.getBoundingClientRect().height);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(panel);
    return () => {
      observer.disconnect();
      report(0);
    };
  }, [docked, onDockedHeightChange, panelRef]);
}
