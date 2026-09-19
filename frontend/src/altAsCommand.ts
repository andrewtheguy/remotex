// PC keyboard, Mac remote: the left Alt key goes out as Command.
//
// Apple's Screen Sharing server reads `Super_L`/`Super_R` as Command and
// `Meta_L`/`Meta_R` as Option, so the engine sends a keyboard's Alt codes as
// Meta and leaves the Windows keys as Super (`keymap::apple_keysym`): a PC
// keyboard reaches a Mac the way the same keyboard plugged into one does, with
// the Windows key as Command and the Alt keys as Option. That leaves Command
// behind the one key a Windows host guards hardest. Windows keeps Super+C for
// itself, beside Super+L and the rest of the Super chords it reserves, so the
// chord never becomes a key event this page can forward and Command-C — copy —
// cannot be typed at the Mac at all.
//
// So the left Alt key is Command here, which is what RealVNC does from a PC
// keyboard by default, and the right one stays Option. Alt+C is an ordinary key
// event on every host, Option is still reachable on the right, and there is
// nothing to choose or store. What it costs is the *left* Option key, which the
// soft keyboard still carries: its chords are sent by code and never come
// through here.
//
// The Windows keys move with it, onto the right Command, for the reason RealVNC
// moves them: three physical keys want a Command and the wire has two codes, so
// two of them must share one. Left Alt and the left Windows key are one hand's
// reach apart, and a key held while its twin is released would lift a Command
// the user is still holding; the two Windows keys are the pair a keyboard is
// least likely to hold together. Measured on macOS 26, the Mac does tell the
// two apart — `Super_L` arrives as left Command and `Super_R` as right — so the
// split is real rather than cosmetic.
//
// This cannot live in the engine's table, where it would be one line: a Mac
// host's left Option key is `AltLeft` as well and must stay Option, and the
// engine is told nothing about the host. The page knows both ends — `isMacHost`
// and the `remoteOs` message — so the substitution belongs here, beside the
// Command translator it is the mirror of (`macKeys.ts`). The two never run
// together: one is a Mac host driving a non-Mac remote, this is a non-Mac host
// driving a Mac.

const AS_COMMAND: ReadonlyMap<string, string> = new Map([
  ["AltLeft", "MetaLeft"], // Super_L: left Command
  ["MetaLeft", "MetaRight"], // Super_R: right Command
  ["MetaRight", "MetaRight"],
]);

/// The code to put on the wire for a physical key. Everything the table does not
/// name — the right Alt key with it, which stays Option — goes out as itself.
export function altAsCommand(code: string): string {
  return AS_COMMAND.get(code) ?? code;
}
