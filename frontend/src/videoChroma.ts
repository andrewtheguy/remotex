// How much colour this browser's video streams carry: the one question about its
// decoders the gateway is told, asked once, before the client mounts.
//
// The gateway streams VP9. Profile 1 (4:4:4) is the picture worth having on a
// desktop — a coloured glyph stem in profile 0 shares its one colour sample with
// three background pixels and comes back at a quarter of its saturation, which no
// quantizer recovers — but no hardware VP9 decoder takes profile 1, and a browser
// with no software VP9 at all, which is iOS and iPadOS, refuses it by name. The
// gateway cannot tell which it is talking to, so the browser says: `chooseVideoChroma`
// asks `VideoDecoder.isConfigSupported` about one representative 4:4:4 configuration,
// and the answer rides every session socket this page opens (`gateway.ts`), which is
// what makes it known both when a target is picked and when a takeover reconnects the
// selected one for this browser — before any message the client could send.
//
// A target that sets `render_chroma` overrules all of this and streams what it names;
// the answer resolves the targets that name nothing, which is what lets one target
// serve both kinds of browser.
//
// This is *selection*, never refusal, and that distinction is what an earlier probe
// lacked and was removed for. `isConfigSupported` is not reliable enough to refuse a
// browser on — it has answered the same question differently on the same browser — so
// nothing here turns a "no" into a closed door: a "no" asks for 4:2:0, which every VP9
// decoder takes, a "yes" or an exception asks for 4:4:4, and a browser that then cannot
// decode what it asked for is told so by its own decoder at `configure`, by name
// (videoDecoder.ts). One question at page load, not a round trip in front of every
// session.

/** The two chroma samplings the gateway encodes, spelled as the wire spells them. */
export type VideoChroma = "444" | "420";

/**
 * The configuration the question is asked about: VP9 profile 1, level 4.0, eight
 * bits, 4:4:4, BT.601 studio swing — a 1080p desktop, and the shape of every string
 * the gateway announces for a 4:4:4 stream (`codec_string` in src/vp9.rs). Every
 * field is spelled out because the defaults are wrong for this stream: an omitted
 * chroma field means 4:2:0, which Chromium reads as 4:2:2 on a profile 1 string.
 *
 * Profile 0 is deliberately not asked about. It is the answer a "no" here selects,
 * every VP9 decoder takes it, and a browser that decodes neither is one `preflight.ts`
 * has already turned away for having no `VideoDecoder` worth the name.
 */
const VP9_444_PROBE = "vp09.01.40.08.03.06.06.06.00";

let chosen: VideoChroma | null = null;

/**
 * Ask the browser once, and remember the answer for `videoChroma()`.
 *
 * Only a definite "no" selects 4:2:0. An exception — a browser whose
 * `isConfigSupported` rejects a valid string, which has happened — is not a "no", and
 * reads as 4:4:4: the honest failure for a wrong guess is the decoder's own, and 4:4:4
 * is the picture the fleet is meant to get.
 */
export async function chooseVideoChroma(): Promise<VideoChroma> {
  if (chosen) {
    return chosen;
  }
  let supported = true;
  try {
    const answer = await VideoDecoder.isConfigSupported({
      codec: VP9_444_PROBE,
    });
    supported = answer.supported !== false;
  } catch {
    supported = true;
  }
  chosen = supported ? "444" : "420";
  return chosen;
}

/**
 * The chroma this browser asked for. Only valid after `chooseVideoChroma` has
 * resolved, which `main.tsx` awaits before mounting; asking earlier is a programming
 * error, not a case to have a default for.
 */
export function videoChroma(): VideoChroma {
  if (!chosen) {
    throw new Error("videoChroma() before chooseVideoChroma() resolved");
  }
  return chosen;
}

/** Test seam: forget the answer so the question can be asked again. */
export function resetVideoChromaForTests(): void {
  chosen = null;
}
