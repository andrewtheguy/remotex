// What this session's moving pixels are being carried by, for the Help card.
//
// A module of its own rather than a helper inside `FloatingMenu.tsx`, because it is a
// pure function of two arguments and the component around it is not: importing that
// file runs `gateway.ts` and `useRemoteDesktop.ts`, both of which read `window` and
// `screen` at module load. A test of this string should not have to stand up a fake
// browser to reach it.
//
// Worth showing because it is the one property of a session nothing else reveals: the
// codec is the operator's, set per target, and a picture that decodes says nothing
// about which one is decoding it. When one does *not* decode, this row and the error
// are the only two places the answer appears.
//
// Three states, and they are genuinely different rather than degrees of the same one:
// a target that streams nothing, one that is connected but has not yet produced a
// frame, and one that is streaming. The last shows the exact `VideoDecoder.configure`
// strings too, because the family alone hides the profile and level a decoder was built
// with — which is what a "this browser cannot decode…" report turns on. A `motion`
// target runs a stream per moving region and its regions differ in size, so there may
// be several.
export function videoLabel(
  codec: string | null,
  decodeStrings: string[],
): string {
  if (!codec) {
    return "None — this target sends tiles";
  }
  const family = codec.toUpperCase();
  if (decodeStrings.length === 0) {
    return `${family} — waiting for the first frame`;
  }
  return `${family} — ${decodeStrings.join(", ")}`;
}
