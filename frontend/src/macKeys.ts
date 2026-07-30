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
//   - **Five chords are missing from the table.** The viewer also maps Command
//     plus L, N, O, T and W, because `NSEvent.addLocalMonitorForEvents` sees
//     every chord the app is given. A web page does not: Command-W closes the
//     tab, Command-T and Command-N open one, Command-L goes to the address bar
//     and Command-O opens a file dialog, and `preventDefault()` does not reach
//     any of them in either Chrome or Safari. Mapping them would not send
//     Control-W to the guest, it would end the session — strictly worse than
//     leaving them alone. Command-R is preventable and was still left out: it
//     would work until some browser version where it doesn't, and the failure is
//     the SPA reloading out from under a live session.
//   - **Shift, Control and Option are told apart from ordinary keys.** AppKit
//     reports them as `flagsChanged`, a branch that never reaches the viewer's
//     `translateKey`; a browser reports them as ordinary key events. Without the
//     distinction, the Shift in Command-Shift-Z looked like a key outside the
//     table and forwarded Command, so redo arrived as Meta-Shift-Control-Z.
//   - **A release flush on Command up.** macOS browsers can withhold `keyup` for
//     a character key while Command is held. The viewer never sees that because
//     it reads `NSEvent`s. Here a missing `keyup` would strand both the letter
//     *and* the synthetic Control down on the guest, so Command's own release
//     flushes anything still held.
//
// Command-V does not push the local clipboard the way the viewer's does
// (`KeyboardCapture.swift:149-155`). `useRemoteDesktop` already pushes on focus
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

const META_CODES: ReadonlySet<string> = new Set(["MetaLeft", "MetaRight"]);

// Modifiers that qualify a chord rather than being its key. They must not be
// mistaken for "some key that isn't in the table", which is what decides that
// Command should be forwarded as itself: Command-Shift-Z is a mapped chord with a
// Shift on it, and forwarding Command there sent the guest Meta-Shift-Control-Z.
//
// The viewer needs no such set. AppKit reports these as `flagsChanged`, a
// separate branch that never reaches its `translateKey`; a browser reports them
// as ordinary key events, so the distinction has to be made here.
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
    if (pressed && meta && COMMAND_MAPS_TO_CONTROL.has(code)) {
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
