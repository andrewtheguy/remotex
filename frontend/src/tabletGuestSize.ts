// The size a tablet asks its remote to be.
//
// Phone or tablet is told apart off the screen's short side in CSS pixels, by
// deliberately the crudest test that separates them: the largest phone is around
// 440 CSS px across its short side and the smallest tablet around 740, so the
// boundary sits in a gap no real device occupies and nothing near it has an
// answer worth getting right.
export const TABLET_MIN_SHORT_SIDE = 600;

/** A rectangle in CSS pixels: the screen, or the layout viewport. */
export interface CssSize {
  readonly w: number;
  readonly h: number;
}

/**
 * A tablet's guest size, or null for a phone, which asks for the target default.
 *
 * A tablet asks for its screen's landscape dimensions — the screen rather than
 * the window, so rotating does not re-ask: the desktop is landscape-shaped in
 * either orientation, and fit-to-width plus pan cover the rest.
 *
 * Less the band the browser keeps above the page. An iPad's status bar stays
 * above an installed page, and Safari's bar above a tab, so the page is that much
 * shorter than the screen; with the width fitting at 1:1 in landscape, a desktop
 * as tall as the screen overhangs by exactly that band and the taskbar sits under
 * the fold. The band is the gap between the screen's height along the current
 * orientation and the layout viewport's. It is the same in either orientation on
 * an iPad, which is what lets a portrait reading stand for the landscape one.
 *
 * The gap is read only while the page spans the screen's width — the one case in
 * which the remainder is the browser's own. A windowed page (Stage Manager, split
 * view) falls short on both axes and says nothing about the status bar, so it
 * keeps the screen's full height. So does a page that fills the screen outright:
 * there the gap is zero and the full height stands, unchanged.
 */
export function tabletGuestSize(
  screen: CssSize,
  viewport: CssSize,
): CssSize | null {
  const long = Math.max(screen.w, screen.h);
  const short = Math.min(screen.w, screen.h);
  if (short < TABLET_MIN_SHORT_SIDE) {
    return null;
  }
  const landscape = viewport.w >= viewport.h;
  const [screenW, screenH] = landscape ? [long, short] : [short, long];
  // Within a pixel, as the canvas snap is: a fractional density can round the
  // viewport a pixel off the screen without any chrome being involved.
  const spansScreen = Math.abs(viewport.w - screenW) <= 1;
  const band = spansScreen ? Math.max(0, screenH - viewport.h) : 0;
  return { w: long, h: Math.max(1, short - band) };
}
