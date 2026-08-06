// What this session is actually speaking, for the "This session" card.
//
// A module of its own because it is a pure function of two strings, where the
// component that shows it cannot be imported without a browser — importing
// `FloatingMenu.tsx` runs modules that read `window` at load, and a test of this
// string should not have to stand up a fake browser to reach it.
//
// Worth showing because `vnc` is three different things. A plain VNC server, a Mac
// in Screen Sharing's Standard mode and a Mac in High Performance mode all arrive
// as `"protocol":"vnc"`, and they differ in what a person will notice: Standard
// shares the Mac's physical displays and refuses resize, High Performance replaces
// them with one virtual display and is reverse engineered from end to end. "Why is
// there no display list", "why is Resize to Window greyed", "why did the desktop
// come back wrong after a resize" all have the same first question — which of the
// three is this — and until this row existed the answer was in the operator's
// config file, which whoever is looking at the screen generally does not have.
//
// The config spelling is kept in the label rather than translated away. It is what
// `subtype` is set to in `remotex.toml`, so somebody reading this can find the line
// that produced it, and a subtype this build has never heard of still names itself
// instead of vanishing.
export function connectionLabel(
  protocol: string,
  subtype: string | null,
): string {
  const family = protocol.toUpperCase();
  if (!subtype) {
    return family;
  }
  const known: Record<string, string> = {
    ard: "Apple Screen Sharing, Standard mode",
    "ard-high-performance":
      "Apple Screen Sharing, High Performance — experimental",
  };
  const described = known[subtype];
  return described
    ? `${family} · ${described} (${subtype})`
    : `${family} · ${subtype}`;
}

/**
 * The same fact, for a line that also has to carry a host and a port: the target
 * picker's row.
 *
 * The config spelling alone, without the prose. What that row is for is choosing
 * between machines, and `VNC · ard · 192.0.2.10:5900` is a choice somebody can
 * make at a glance where the sentence above it would wrap. The prose belongs on
 * the session card, which is one target and has the room.
 */
export function connectionShortLabel(
  protocol: string,
  subtype: string | null,
): string {
  const family = protocol.toUpperCase();
  return subtype ? `${family} · ${subtype}` : family;
}
