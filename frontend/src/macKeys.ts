// Mac host, non-Mac guest: turn local Command chords into the Control chords the
// guest expects. A port of the macOS viewer's `KeyboardTranslator` (see
// apps/remotex-viewer/Sources/Input/KeyboardTranslator.swift) — same state
// machine, same emitted vocabulary, deliberately a smaller mapping table.
//
// The two clients can share the logic because they already share the alphabet:
// the viewer maps macOS virtual keycodes to DOM `code` strings, which is what a
// `KeyboardEvent` hands us directly. Neither the wire format nor the gateway
// knows this file exists — a translated chord is indistinguishable from one the
// user really typed.
//
// What differs from the viewer, and why:
//
//   - **Six chords are only in the table for a host that is given them.** Command
//     plus L, N, O, R, T and W are mapped when this page runs inside `remotex.app`,
//     which drops its own menu accelerators while the desktop has focus and so is
//     given every chord; in a Chrome app window, which reserves no keys at all
//     (`appWindow.ts`); and in a plain tab only while a keyboard lock is held — see
//     `immersive.ts` and `setCapturesEveryChord`. A lock is the only one of the three
//     that changes under a running session, which is why there is a setter at all.
//     In an unlocked tab a browser does not get them: Command-W closes the tab,
//     Command-T and Command-N open one, Command-L goes to the address bar and
//     Command-O opens a file dialog, and `preventDefault()` does not reach any of
//     them in either Chrome or Safari. Mapping them there would not send Control-W
//     to the guest, it would end the session — strictly worse than leaving them
//     alone. Command-R is preventable and is still left out of the *unlocked tab's*
//     set: it would work until some browser version where it doesn't, and the failure
//     is the SPA reloading out from under a live session. It joins the other five
//     wherever the window says outright that it will not act on a chord — a keyboard
//     lock, or an app window. See `setCapturesEveryChord`.
//   - **Shift, Control and Option are told apart from ordinary keys.** They arrive
//     as ordinary key events, not as a separate modifier-changed report. Without the
//     distinction, the Shift in Command-Shift-Z looked like a key outside the
//     table and forwarded Command, so redo arrived as Meta-Shift-Control-Z.
//   - **A release flush on Command up.** macOS can withhold `keyup` for a
//     character key while Command is held. A missing `keyup` would strand both the
//     letter *and* the synthetic Control down on the guest, so Command's own
//     release flushes anything still held.
//
// Command-V does not read the local clipboard on its way past.
// `useRemoteDesktop` already pushes on focus
// and visibility change, which covers the same case — copy elsewhere, come back,
// paste — and a `readText()` on every Command-V would put Safari's paste
// confirmation in front of a user who is typing.

export type TranslatedKey = {
  code: string;
  pressed: boolean;
  caps: boolean;
};

/// One keyboard event as this translator needs to see it. `meta` is the event's
/// own `metaKey`, not something inferred from the presses seen so far: Command
/// may already have been down when the surface took focus.
export type SourceKey = {
  code: string;
  pressed: boolean;
  caps: boolean;
  meta: boolean;
};

// Command chords that become Control chords. Every one of these is delivered to
// a page by both Chrome and Safari on macOS, and every one means Control plus
// the same letter on Windows and Linux. The header explains what is missing.
const COMMAND_MAPS_TO_CONTROL: ReadonlySet<string> = new Set([
  "KeyA", // select all
  "KeyC", // copy
  "KeyF", // find
  "KeyP", // print
  "KeyS", // save
  "KeyV", // paste
  "KeyX", // cut
  "KeyZ", // undo
]);

/// The six the header says a browser keeps for itself, and the whole of what a
/// host can add.
///
/// Exported because two things must agree on exactly this list: the table below,
/// and the code list `immersive.ts` hands `navigator.keyboard.lock`. A table that
/// promises a translation the browser eats is a chord that silently closes the tab,
/// so they are the same six by construction rather than by two people remembering.
export const BROWSER_RESERVED_CHORD_CODES: readonly string[] = [
  "KeyL", // address bar in a browser; focus/lock in a guest
  "KeyN", // new window
  "KeyO", // open
  "KeyR", // reload
  "KeyT", // new tab
  "KeyW", // close
];

// The six above, added back for the hosts that do receive them.
//
// `remotex.app` drops its menu accelerators while a live desktop has focus, so ⌘W
// closes nothing of the app's and ⌘L goes nowhere — the chord arrives in this page
// as an ordinary `keydown`. A browser gets the same thing, but only while a keyboard
// lock is held, which is why this is a table that can be swapped rather than one
// chosen for good at construction. Either way it is one set rather than a second
// translator: the state machine, the bare-Command tap and the release bookkeeping are
// all the same code, tested once.
const COMMAND_MAPS_TO_CONTROL_NATIVE: ReadonlySet<string> = new Set([
  ...COMMAND_MAPS_TO_CONTROL,
  ...BROWSER_RESERVED_CHORD_CODES,
]);

const META_CODES: ReadonlySet<string> = new Set(["MetaLeft", "MetaRight"]);

// Modifiers that qualify a chord rather than being its key. They must not be
// mistaken for "some key that isn't in the table", which is what decides that
// Command should be forwarded as itself: Command-Shift-Z is a mapped chord with a
// Shift on it, and forwarding Command there sent the guest Meta-Shift-Control-Z.
//
// Both hosts report these as ordinary key events, so the distinction has to be
// made here rather than falling out of how they arrive.
const CHORD_MODIFIERS: ReadonlySet<string> = new Set([
  "ShiftLeft",
  "ShiftRight",
  "ControlLeft",
  "ControlRight",
  "AltLeft",
  "AltRight",
]);

/// Whether this browser is running on a Mac, and so whether Command chords are
/// the local convention at all.
///
/// iPadOS reports itself as a Mac and is deliberately not excluded: an iPad with
/// a hardware keyboard has a Command key that means what a Mac's does, and the
/// on-screen keyboard never produces one.
export function isMacHost(): boolean {
  const data = (
    navigator as Navigator & { userAgentData?: { platform?: string } }
  ).userAgentData;
  if (data?.platform) {
    return data.platform === "macOS";
  }
  return /Mac/i.test(navigator.platform || navigator.userAgent);
}

export class MacKeyboardTranslator {
  // Command is down but has not been forwarded to the remote — it may yet turn
  // out to be a bare tap, a translated chord, or a forwarded one, and which it
  // is is only known once another key arrives or Command comes back up.
  private pendingCommandCodes = new Set<string>();
  // Command was forwarded as itself (an unmapped chord), so its release has to
  // be forwarded too or it sticks down on the guest.
  private forwardedCommandCodes = new Set<string>();
  // Command took part in a chord, so its release must not also tap Meta.
  private commandWasUsed = false;
  // Keys currently held that were sent as Control chords, so their release is
  // sent for the same code and the synthetic Control is lifted after the last.
  private translatedCommandKeys = new Set<string>();
  private syntheticControlHeld = false;
  private mappedChords: ReadonlySet<string> = COMMAND_MAPS_TO_CONTROL;

  /// `capturesEveryChord` is the host's answer to "am I given ⌘W?" — false for a
  /// browser, true for `remotex.app`, which takes keys before the menu bar. It
  /// selects the chord table and changes nothing else: a chord outside the table is
  /// forwarded as Meta either way, so the two hosts differ in what they can offer
  /// rather than in how they behave.
  constructor(capturesEveryChord = false) {
    this.setCapturesEveryChord(capturesEveryChord);
  }

  /// Change that answer mid-session.
  ///
  /// `remotex.app` answers once, for the session: it drops its menu accelerators for
  /// as long as a live desktop has focus. A browser answers per keyboard lock, which
  /// the user enters and leaves and which a held Esc can end at any moment — so the
  /// table has to be able to move under a running session.
  ///
  /// Safe to call with a chord half-way through, and that is not luck: the table is
  /// consulted only when a key goes *down* (see `translateKey`). Every release path
  /// runs off `translatedCommandKeys` and `forwardedCommandCodes`, so a key pressed
  /// under one table is always released under the rules it was pressed with, and the
  /// synthetic Control is lifted either way.
  setCapturesEveryChord(capturesEveryChord: boolean): void {
    this.mappedChords = capturesEveryChord
      ? COMMAND_MAPS_TO_CONTROL_NATIVE
      : COMMAND_MAPS_TO_CONTROL;
  }

  /// Translate one event into what should go on the wire, in order.
  ///
  /// `mapCommandToControl` false is a pass-through: the caller's own `code` goes
  /// out unchanged, which is what a Mac guest wants (Command-V should arrive as
  /// Command-V) and what the preference turns off.
  translate(event: SourceKey, mapCommandToControl: boolean): TranslatedKey[] {
    const { code, pressed, caps } = event;

    // Browsers report CapsLock as a press when it engages and a release when it
    // disengages, never as a tap. The guest wants the tap, same as the viewer.
    if (code === "CapsLock") {
      return [
        { code, pressed: true, caps },
        { code, pressed: false, caps },
      ];
    }

    if (!mapCommandToControl) {
      return [{ code, pressed, caps }];
    }

    if (META_CODES.has(code)) {
      return this.translateCommand(code, pressed, caps);
    }
    return this.translateKey(code, pressed, caps, event.meta);
  }

  /// A chord the client took for itself while Command was held. The key Command
  /// was held with never arrived here, so without this Command's release reads as
  /// a bare tap and the guest is handed the Windows key — the SPA's own
  /// Ctrl+Cmd+Shift+; would hide its toolbar *and* open a Start menu.
  ///
  /// Only while Command is actually pending: setting the flag at any other moment
  /// would swallow the next real bare tap instead.
  noteCommandUsedLocally(): void {
    if (this.pendingCommandCodes.size > 0) {
      this.commandWasUsed = true;
    }
  }

  /// Forget everything held. The caller pairs this with its own release sweep,
  /// so nothing here emits: a chord half-way through must not be resumed against
  /// a remote whose releases have already been sent.
  reset(): void {
    this.pendingCommandCodes.clear();
    this.forwardedCommandCodes.clear();
    this.translatedCommandKeys.clear();
    this.commandWasUsed = false;
    this.syntheticControlHeld = false;
  }

  private translateCommand(
    code: string,
    pressed: boolean,
    caps: boolean,
  ): TranslatedKey[] {
    if (pressed) {
      this.pendingCommandCodes.add(code);
      return [];
    }

    // Command is up, so anything it was holding down is over. This is the guard
    // the viewer does not need: without it a swallowed `keyup` leaves the letter
    // and the synthetic Control pressed on the guest forever.
    const translated = this.flushHeldTranslations(caps);

    const wasPending = this.pendingCommandCodes.delete(code);
    if (this.forwardedCommandCodes.delete(code)) {
      // Clearing here as well as below, not only there: this path returns early,
      // and leaving the flag set swallows the synthetic tap for the *next*
      // standalone Command press.
      if (this.pendingCommandCodes.size === 0) {
        this.commandWasUsed = false;
      }
      translated.push({ code, pressed: false, caps });
      return translated;
    }
    if (!wasPending) {
      return translated;
    }
    if (!this.commandWasUsed && this.pendingCommandCodes.size === 0) {
      // A bare Command tap. Meaningless to a Mac guest's own Command, useful to
      // a Windows one: this is how the Start menu opens.
      translated.push({ code, pressed: true, caps });
      translated.push({ code, pressed: false, caps });
      return translated;
    }
    if (this.pendingCommandCodes.size === 0) {
      this.commandWasUsed = false;
    }
    return translated;
  }

  private translateKey(
    code: string,
    pressed: boolean,
    caps: boolean,
    meta: boolean,
  ): TranslatedKey[] {
    // Straight through, and deliberately before everything below: a modifier is
    // not a chord's key, so it neither starts a translation nor forwards Command.
    if (CHORD_MODIFIERS.has(code)) {
      return [{ code, pressed, caps }];
    }
    if (pressed && meta && this.mappedChords.has(code)) {
      return this.beginTranslatedChord(code, caps);
    }
    if (!pressed && this.translatedCommandKeys.delete(code)) {
      return this.endTranslatedChord(code, caps);
    }
    const translated = pressed && meta ? this.forwardPendingCommands(caps) : [];
    translated.push({ code, pressed, caps });
    return translated;
  }

  /// Command plus a mapped letter: the guest is told Control instead, and the
  /// Command press it never saw stays unsent.
  private beginTranslatedChord(code: string, caps: boolean): TranslatedKey[] {
    this.commandWasUsed = true;
    this.translatedCommandKeys.add(code);
    // A Command this hold already forwarded is taken back first. The two modes
    // are mutually exclusive: Meta held *and* a synthetic Control is a chord
    // nobody typed, and it is what the guest saw for Command-B then Command-C.
    const translated = this.withdrawForwardedCommands(caps);
    if (!this.syntheticControlHeld) {
      this.syntheticControlHeld = true;
      translated.push({ code: "ControlLeft", pressed: true, caps });
    }
    translated.push({ code, pressed: true, caps });
    return translated;
  }

  /// Release every Command already forwarded as itself, without forgetting it is
  /// physically down: the codes stay in `pendingCommandCodes`, so Command's own
  /// release is still accounted for and a later unmapped key forwards it again.
  private withdrawForwardedCommands(caps: boolean): TranslatedKey[] {
    const translated: TranslatedKey[] = [];
    for (const commandCode of this.forwardedCommandCodes) {
      translated.push({ code: commandCode, pressed: false, caps });
    }
    this.forwardedCommandCodes.clear();
    return translated;
  }

  /// The letter of a translated chord came back up. The synthetic Control follows
  /// it, but only once no other translated key is still down — Command-C then
  /// Command-V without releasing Command holds one Control across both.
  private endTranslatedChord(code: string, caps: boolean): TranslatedKey[] {
    const translated: TranslatedKey[] = [{ code, pressed: false, caps }];
    if (this.translatedCommandKeys.size === 0) {
      translated.push(...this.releaseSyntheticControl(caps));
    }
    return translated;
  }

  /// An unmapped chord: Command goes out as itself, so the guest sees a Meta
  /// chord rather than a lone letter. Command's own press was withheld until now
  /// precisely so a chord that turns out to be mapped never sends one.
  private forwardPendingCommands(caps: boolean): TranslatedKey[] {
    this.commandWasUsed = true;
    // The same exclusivity from the other side: a mapped key still held under
    // this Command loses its synthetic Control before Command goes out as itself.
    const translated = this.releaseSyntheticControl(caps);
    for (const commandCode of this.pendingCommandCodes) {
      if (this.forwardedCommandCodes.has(commandCode)) {
        continue;
      }
      this.forwardedCommandCodes.add(commandCode);
      translated.push({ code: commandCode, pressed: true, caps });
    }
    return translated;
  }

  /// Releases for translated keys still held, newest first, plus the synthetic
  /// Control once the last of them is up.
  private flushHeldTranslations(caps: boolean): TranslatedKey[] {
    if (this.translatedCommandKeys.size === 0) {
      return [];
    }
    const translated: TranslatedKey[] = [];
    for (const held of this.translatedCommandKeys) {
      translated.push({ code: held, pressed: false, caps });
    }
    this.translatedCommandKeys.clear();
    translated.push(...this.releaseSyntheticControl(caps));
    return translated;
  }

  /// The synthetic Control's release, or nothing if it is not held. Every path
  /// that stops translating goes through here, so none of them can leave a
  /// Control down that the guest was told about and never told to let go.
  private releaseSyntheticControl(caps: boolean): TranslatedKey[] {
    if (!this.syntheticControlHeld) {
      return [];
    }
    this.syntheticControlHeld = false;
    return [{ code: "ControlLeft", pressed: false, caps }];
  }
}
