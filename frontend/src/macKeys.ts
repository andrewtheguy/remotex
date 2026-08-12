// Mac host, non-Mac guest: turn local Command chords into the Control chords the
// guest expects. This is entirely a browser concern: neither the wire format nor
// the gateway knows this file exists, and a translated chord is indistinguishable
// from one the user really typed.
//
// What differs from the viewer, and why:
//
//   - **The full Command table is always active.** A Chrome app window delivers the
//     six chords a normal browser tab reserves, and an automatic Keyboard Lock
//     supplies them to a fullscreen tab. A windowed tab never sends
//     those keydowns to the page, so including them here changes nothing there; it
//     means every chord that does arrive has one stable meaning regardless of window
//     or fullscreen state.
//   - **Shift, Control and Option are told apart from ordinary keys.** They arrive
//     as ordinary key events, not as a separate modifier-changed report. Without the
//     distinction, the Shift in Command-Shift-Z looked like a key outside the
//     table and forwarded Command, so redo arrived as Meta-Shift-Control-Z.
//   - **A release flush on Command up.** macOS can withhold `keyup` for a
//     character key while Command is held. A missing `keyup` would strand both the
//     letter *and* the synthetic Control down on the guest, so Command's own
//     release flushes anything still held — translated chords, letters forwarded
//     as themselves, and the pass-through mode a Mac guest uses alike. The
//     withholding is a fact about the local browser, not the guest, so no mode is
//     exempt: a stranded Q turns every ⌘Q after the first into an auto-repeat of
//     a key the guest thinks is still down, which fires nothing.
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

// Command chords delivered to every page that become Control chords. The browser-
// reserved additions live below so Keyboard Lock can use the exact same list.
const COMMON_COMMAND_MAPS_TO_CONTROL: readonly string[] = [
  "KeyA", // select all
  "KeyC", // copy
  "KeyF", // find
  "KeyP", // print
  "KeyS", // save
  "KeyV", // paste
  "KeyX", // cut
  "KeyZ", // undo
];

/// The six the header says a browser keeps for itself, and the whole of what a
/// host can add.
///
/// Exported because the translation table and the automatic Keyboard Lock must agree
/// on exactly this list.
export const BROWSER_RESERVED_CHORD_CODES: readonly string[] = [
  "KeyL", // address bar in a browser; focus/lock in a guest
  "KeyN", // new window
  "KeyO", // open
  "KeyR", // reload
  "KeyT", // new tab
  "KeyW", // close
];

const COMMAND_MAPS_TO_CONTROL: ReadonlySet<string> = new Set([
  ...COMMON_COMMAND_MAPS_TO_CONTROL,
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
  // Non-modifier keys sent to the guest as themselves while Command was down — a
  // forwarded chord's letter, or any key in pass-through mode. Their keyups are
  // the ones macOS may withhold, so Command's release sweeps whatever is left.
  private heldUnderCommand = new Set<string>();

  /// Translate one event into what should go on the wire, in order.
  ///
  /// `mapCommandToControl` false is a pass-through: the caller's own `code` goes
  /// out unchanged, which is what a Mac guest wants (Command-V should arrive as
  /// Command-V) and what the preference turns off. The flush on Command up still
  /// applies there — the withheld keyup is the local browser's doing, and a Mac
  /// guest's ⌘Q is exactly the chord that hit it.
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
      if (META_CODES.has(code) && !pressed) {
        const translated = this.flushHeldUnderCommand(caps);
        translated.push({ code, pressed, caps });
        return translated;
      }
      this.noteHeldUnderCommand(code, pressed, event.meta);
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
    this.heldUnderCommand.clear();
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
    // and the synthetic Control pressed on the guest forever. Both kinds of held
    // key are swept — the translated ones, and the ones forwarded as themselves.
    const translated = this.flushHeldTranslations(caps);
    translated.push(...this.flushHeldUnderCommand(caps));

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
    if (pressed && meta && COMMAND_MAPS_TO_CONTROL.has(code)) {
      return this.beginTranslatedChord(code, caps);
    }
    if (!pressed && this.translatedCommandKeys.delete(code)) {
      return this.endTranslatedChord(code, caps);
    }
    this.noteHeldUnderCommand(code, pressed, meta);
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

  /// Follows a key that went to the guest as itself, whichever mode sent it.
  /// Modifiers stay out: their keyups arrive even under Command, and sweeping a
  /// Shift the user is still holding would take it off the guest mid-chord.
  /// Command itself stays out too — in pass-through mode its own keydown reports
  /// `meta` and would otherwise sweep itself.
  private noteHeldUnderCommand(
    code: string,
    pressed: boolean,
    meta: boolean,
  ): void {
    if (CHORD_MODIFIERS.has(code) || META_CODES.has(code)) {
      return;
    }
    if (pressed && meta) {
      this.heldUnderCommand.add(code);
    } else if (!pressed) {
      this.heldUnderCommand.delete(code);
    }
  }

  /// Releases for keys sent as themselves under a Command that is now up, or
  /// nothing when their keyups arrived normally. A keyup that turns up late —
  /// Command let go first, the letter after — goes out as a second release,
  /// which every guest treats as a no-op.
  private flushHeldUnderCommand(caps: boolean): TranslatedKey[] {
    const translated: TranslatedKey[] = [];
    for (const held of this.heldUnderCommand) {
      translated.push({ code: held, pressed: false, caps });
    }
    this.heldUnderCommand.clear();
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
