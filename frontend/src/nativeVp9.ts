// Whether this browser's own `VideoDecoder` takes VP9 at 4:4:4, asked once,
// before the client mounts, and stated on every session socket the page opens
// (gateway.ts).
//
// 4:4:4, profile 1, is the picture worth having on a desktop: a coloured glyph
// stem at 4:2:0 shares its one colour sample with three background pixels and
// comes back at a quarter of its saturation, which no quantizer recovers. But no
// browser's hardware VP9 path takes profile 1, so a browser decodes it in software
// where it has a software VP9 decoder, and a browser with none, which is iOS and
// iPadOS, refuses it by name. So the answer decides what a `wlshare` target sends:
// 4:4:4 to a browser that takes it, 4:2:0, profile 0, which every decoder takes, to
// one that does not. The gateway's own encoding is 4:2:0 for every browser, and no
// decoder of the page's stands in for the browser's.
//
// `askNativeVp9` asks `VideoDecoder.isConfigSupported` about one representative
// 4:4:4 configuration. Only a definite "yes" is sent 4:4:4: a stream this
// browser's decoder then refuses has nothing to fall back on, where 4:2:0 sent
// to a browser that would have taken more costs only colour, so an answer with no
// verdict and an exception read as "no". One question at page load, not a round
// trip in front of every session.

/**
 * The configuration the question is asked about: VP9 profile 1, level 4.0, eight
 * bits, 4:4:4, BT.601 studio swing — a 1080p desktop, and the shape of every 4:4:4
 * string the gateway announces (`codec_string` in screen-vp9). Every field is
 * spelled out because the defaults are wrong for this stream: an omitted chroma
 * field means 4:2:0, which Chromium reads as 4:2:2 on a profile 1 string.
 */
const VP9_444_PROBE = "vp09.01.40.08.03.06.06.06.00";

let answer: boolean | null = null;

/** Ask the browser once, and remember the answer for `nativeVp9()`. */
export async function askNativeVp9(): Promise<boolean> {
  if (answer !== null) {
    return answer;
  }
  let supported = false;
  try {
    const reply = await VideoDecoder.isConfigSupported({
      codec: VP9_444_PROBE,
    });
    supported = reply.supported === true;
  } catch {
    supported = false;
  }
  answer = supported;
  return answer;
}

/**
 * Whether the browser's own decoder takes VP9 at 4:4:4. Only valid after
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
