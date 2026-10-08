// How much colour this browser's video streams carry, and what decodes them: the
// one question about its decoders the gateway is told, asked once, before the
// client mounts.
//
// The gateway streams VP9. Profile 1 (4:4:4) is the picture worth having on a
// desktop — a coloured glyph stem in profile 0 shares its one colour sample with
// three background pixels and comes back at a quarter of its saturation, which no
// quantizer recovers — but no browser's hardware VP9 path takes profile 1, so it
// decodes in software, and a browser with no software VP9 at all, which is iOS and
// iPadOS, refuses it by name. The gateway cannot tell which it is talking to, so
// the browser says: `chooseVideoChroma`
// asks `VideoDecoder.isConfigSupported` about one representative 4:4:4 configuration,
// and the answer rides every session socket this page opens (`gateway.ts`), which is
// what makes it known both when a target is picked and when a reattach has to start the
// selected one over — before any message the client could send.
//
// BETA: on a gateway that sets `[vp9_wasm].enabled`, a browser that says no to
// profile 1 is not left with 4:2:0 where the page can decode profile 1 itself:
// andrewtheguy/vp9-wasm's decoder, in the bundle (softwareDecoder.ts), on a page
// that runs shared-memory SIMD WebAssembly and presents its pictures on a WebGL 2
// canvas (softwareSupport.ts). Such a page asks for 4:4:4 and decodes it in the
// module. The key is off unless set, and the module is then never loaded: it is
// there for an operator to compare, in use, 4:4:4 decoded in the page with the
// 4:2:0 such a browser gets otherwise. `?vp9_decoder=software` in the page's URL
// takes the module even where the browser's own decoder would do, to try it. A
// URL that asks is not answered with another decoder: where the gateway does not
// allow the module, or the page cannot run it, a 4:4:4 stream fails saying which
// (`softwareVp9Refusal`), so that what is on the screen is never mistaken for the
// module's.
//
// A target that sets `render_chroma` overrules all of this and streams what it names;
// the answer resolves the targets that name nothing, which is what lets one target
// serve both kinds of browser. Which decoder a stream gets follows what it is: the
// module decodes profile 1 and nothing else, so a 4:2:0 stream is the browser's own
// decoder's on every page.
//
// This is *selection*, never refusal, and that distinction is what an earlier probe
// lacked and was removed for. `isConfigSupported` is not reliable enough to refuse a
// browser on — it has answered the same question differently on the same browser — so
// nothing here turns a "no" into a closed door: a "no" asks for 4:2:0, which every VP9
// decoder takes, or for 4:4:4 decoded in the module where that is allowed and can
// run; a "yes" or an exception asks for 4:4:4, and a browser that then cannot
// decode what it asked for is told so by its own decoder at `configure`, by name
// (videoDecoder.ts). One question at page load, not a round trip in front of every
// session.

import { runsSoftwareDecoder, softwareRequested } from "./softwareSupport.ts";

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

/**
 * What decodes a 4:4:4 stream: the browser's `VideoDecoder`, or vp9-wasm, or
 * nothing, where the page's URL asked for vp9-wasm and the gateway does not allow
 * it or the page cannot run it.
 */
export type Vp9Decoder = "native" | "software" | "not-enabled" | "cannot-run";

/** What the page asks for, and what it decodes a 4:4:4 stream with. */
export interface VideoChoice {
  chroma: VideoChroma;
  decoder: Vp9Decoder;
}

let chosen: VideoChoice | null = null;

/**
 * Whether the browser's own decoder takes profile 1. Only a definite "no" is one.
 * An exception — a browser whose `isConfigSupported` rejects a valid string, which
 * has happened — is not a "no", and reads as a yes: the honest failure for a wrong
 * guess is the decoder's own, and 4:4:4 is the picture the fleet is meant to get.
 */
async function browserTakesProfile1(): Promise<boolean> {
  try {
    const answer = await VideoDecoder.isConfigSupported({
      codec: VP9_444_PROBE,
    });
    return answer.supported !== false;
  } catch {
    return true;
  }
}

/**
 * Ask the browser once, and remember the answer for `videoChroma()` and
 * `videoDecoder()`.
 *
 * `softwareAllowed` is the gateway's `[vp9_wasm].enabled`, as `/api/config` says
 * it (gatewayConfig.ts). Without it the browser's answer is the whole of it:
 * 4:4:4 unless it says no, and its own decoder either way. With it, a page that
 * can run the module decodes 4:4:4 there when the browser says no, or when the
 * page's URL asks; a page that cannot is as if the gateway had not allowed it,
 * unless its URL asked, which is refused rather than given another decoder.
 */
export async function chooseVideoChroma(
  softwareAllowed: boolean | Promise<boolean>,
): Promise<VideoChoice> {
  if (chosen) {
    return chosen;
  }
  const allowed = await softwareAllowed;
  const software = allowed && runsSoftwareDecoder({ widePrimaries: false });
  if (softwareRequested("vp9_decoder")) {
    chosen = {
      chroma: "444",
      decoder: software ? "software" : allowed ? "cannot-run" : "not-enabled",
    };
  } else if (await browserTakesProfile1()) {
    chosen = { chroma: "444", decoder: "native" };
  } else if (software) {
    chosen = { chroma: "444", decoder: "software" };
  } else {
    chosen = { chroma: "420", decoder: "native" };
  }
  return chosen;
}

function choice(asked: string): VideoChoice {
  if (!chosen) {
    throw new Error(`${asked} before chooseVideoChroma() resolved`);
  }
  return chosen;
}

/**
 * The chroma this browser asked for. Only valid after `chooseVideoChroma` has
 * resolved, which `main.tsx` awaits before mounting; asking earlier is a programming
 * error, not a case to have a default for.
 */
export function videoChroma(): VideoChroma {
  return choice("videoChroma()").chroma;
}

/**
 * What decodes a 4:4:4 stream on this page, read by the paint worker's `init`.
 * Valid when `videoChroma` is.
 */
export function videoDecoder(): Vp9Decoder {
  return choice("videoDecoder()").decoder;
}

/**
 * Why a 4:4:4 stream is not decoded on this page, for one whose URL asked for
 * the module where it is not to be had; null on every other page.
 */
export function softwareVp9Refusal(): string | null {
  switch (choice("softwareVp9Refusal()").decoder) {
    case "not-enabled":
      return "This page's URL asks for the software VP9 decoder (?vp9_decoder=software), which this gateway does not enable ([vp9_wasm]).";
    case "cannot-run":
      return "This page's URL asks for the software VP9 decoder (?vp9_decoder=software), which this browser cannot run.";
    default:
      return null;
  }
}

/** Test seam: forget the answer so the question can be asked again. */
export function resetVideoChromaForTests(): void {
  chosen = null;
}
