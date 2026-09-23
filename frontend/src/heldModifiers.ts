// The modifiers the keyboard is physically holding, checked against what every
// later event says about them.
//
// A modifier's release reaches the remote only if the page hears about it, and
// there are two ways it does: the key's own `keyup`, or the overlay's `blur`.
// The local system can take both. A chord it keeps for itself — a window
// manager's move-window drag, a screenshot shortcut, a task switcher that hands
// focus straight back — swallows the `keyup` while the window never stops being
// the focused one, and the remote is left holding a Command or an Option that
// every keystroke after it arrives wearing.
//
// What the system cannot take is the modifier state on the next event. Every
// key, mouse and wheel event reports which of the four modifier families are
// down *now*, so a held code whose family the event reports as up has been let
// go without a `keyup`, and is released here instead. Only keys a DOM `keydown`
// put down are followed: the soft keyboard's sticky modifiers are held by the
// page, not by a finger, and no event's flags know about them.
export interface ModifierFlags {
  shift: boolean;
  control: boolean;
  alt: boolean;
  meta: boolean;
}

type Family = keyof ModifierFlags;

const FAMILIES: ReadonlyMap<string, Family> = new Map([
  ["ShiftLeft", "shift"],
  ["ShiftRight", "shift"],
  ["ControlLeft", "control"],
  ["ControlRight", "control"],
  ["AltLeft", "alt"],
  ["AltRight", "alt"],
  ["MetaLeft", "meta"],
  ["MetaRight", "meta"],
]);

/// The modifier state an event carries. AltGraph counts as Alt: a layout that
/// makes the right Alt key AltGr reports it under that name alone, and the key
/// is no less held for it. A touch event has no `getModifierState`, and so no
/// AltGraph to report.
export function modifierFlags(
  e: KeyboardEvent | MouseEvent | TouchEvent,
): ModifierFlags {
  return {
    shift: e.shiftKey,
    control: e.ctrlKey,
    alt:
      e.altKey || ("getModifierState" in e && e.getModifierState("AltGraph")),
    meta: e.metaKey,
  };
}

export class HeldModifiers {
  private held = new Map<string, Family>();

  /// A key event, and the held modifiers its flags say are no longer down. The
  /// event's own code is never among them: a release leaves the set before the
  /// flags are read, since they already report it up, and a press joins after,
  /// since whether a modifier's own keydown counts itself is the browser's call.
  key(code: string, pressed: boolean, flags: ModifierFlags): string[] {
    if (!pressed) {
      this.held.delete(code);
    }
    const lapsed = this.lapsed(flags);
    const family = FAMILIES.get(code);
    if (pressed && family) {
      this.held.set(code, family);
    }
    return lapsed;
  }

  /// The held modifiers an event's flags say are no longer down, forgotten as
  /// they are returned so each is released once.
  lapsed(flags: ModifierFlags): string[] {
    const lapsed: string[] = [];
    for (const [code, family] of this.held) {
      if (!flags[family]) {
        lapsed.push(code);
      }
    }
    for (const code of lapsed) {
      this.held.delete(code);
    }
    return lapsed;
  }

  /// Forget everything held, for a caller that has released it all itself.
  clear(): void {
    this.held.clear();
  }
}
