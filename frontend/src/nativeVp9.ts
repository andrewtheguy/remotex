// Whether this browser's own `VideoDecoder` takes the gateway's VP9, asked once,
// before the client mounts. The answer is this page's alone: the gateway is not
// told, and sends every browser the same stream.
//
// The gateway streams VP9 at 4:4:4, profile 1 — the picture worth having on a
// desktop: a coloured glyph stem at 4:2:0 shares its one colour sample with three
// background pixels and comes back at a quarter of its saturation, which no
// quantizer recovers. No browser's hardware VP9 path takes profile 1, so a browser
// decodes it in software where it has a software VP9 decoder, and a browser with
// none, which is iOS and iPadOS, refuses it by name. Such a page decodes the stream
// in its own WebAssembly decoder instead (softwareDecoder.ts): `videoDecoder.ts`
// builds that decoder for a profile 1 stream where this answer is no.
//
// `askNativeVp9` asks `VideoDecoder.isConfigSupported` about one representative
// 4:4:4 configuration. Only a definite "no" hands the stream to the page's
// decoder: `isConfigSupported` has answered the same question differently on the
// same browser, so a "yes", an answer with no verdict and an exception all read
// as the browser's own decoder, and a browser that then cannot decode the stream
// is told so by that decoder at `configure`, by name (videoDecoder.ts). One
// question at page load, not a round trip in front of every session.

/**
 * The configuration the question is asked about: VP9 profile 1, level 4.0, eight
 * bits, 4:4:4, BT.601 studio swing — a 1080p desktop, and the shape of every string
 * the gateway announces (`codec_string` in src/vp9.rs). Every field is spelled out
 * because the defaults are wrong for this stream: an omitted chroma field means
 * 4:2:0, which Chromium reads as 4:2:2 on a profile 1 string.
 */
const VP9_444_PROBE = "vp09.01.40.08.03.06.06.06.00";

let answer: boolean | null = null;

/** Ask the browser once, and remember the answer for `nativeVp9()`. */
export async function askNativeVp9(): Promise<boolean> {
  if (answer !== null) {
    return answer;
  }
  let supported = true;
  try {
    const reply = await VideoDecoder.isConfigSupported({
      codec: VP9_444_PROBE,
    });
    supported = reply.supported !== false;
  } catch {
    supported = true;
  }
  answer = supported;
  return answer;
}

/**
 * Whether the browser's own decoder takes the gateway's VP9. Only valid after
 * `askNativeVp9` has resolved, which `main.tsx` awaits before mounting; asking
 * earlier is a programming error, not a case to have a default for.
 */
export function nativeVp9(): boolean {
  if (answer === null) {
    throw new Error("nativeVp9() before askNativeVp9() resolved");
  }
  return answer;
}

/** Test seam: forget the answer so the question can be asked again. */
export function resetNativeVp9ForTests(): void {
  answer = null;
}
