// Which codec this browser's video streams are encoded in: the one question about
// its decoders the gateway is told, asked once, before the client mounts.
//
// The gateway streams VP9, and falls back to H.264 for a browser whose `VideoDecoder`
// has no VP9 — older Safari on hardware without a VP9 block. It cannot tell which it
// is talking to, so the browser says: `chooseVideoCodec` asks
// `VideoDecoder.isConfigSupported` about one representative VP9 configuration, and
// the answer rides every session socket this page opens (`gateway.ts`), which is what
// makes it known both when a target is picked and when a takeover reconnects the
// selected one for this browser — before any message the client could send.
//
// This is *selection*, never refusal, and that distinction is what an earlier probe
// lacked and was removed for. `isConfigSupported` is not reliable enough to refuse a
// browser on — it has answered the same question differently on the same browser —
// so nothing here turns a "no" into a closed door: a "no" asks for H.264, a "yes" or
// an exception asks for VP9, and a browser that then cannot decode what it asked for
// is told so by its own decoder at `configure`, by name (videoDecoder.ts). One
// question at page load, not a round trip in front of every session.

/** The two codecs the gateway encodes, spelled as the wire spells them. */
export type VideoCodec = "vp9" | "h264";

/**
 * The VP9 configuration the question is asked about: profile 0 (4:2:0), level 4.0,
 * eight bits, BT.601 studio swing — a 1080p desktop, and the shape of every string
 * the gateway announces for a `render_chroma = "420"` target. Profile 1 (4:4:4) is
 * deliberately not asked about: a browser that decodes profile 0 in hardware and
 * profile 1 not at all is a VP9 browser, and a 4:4:4 target it cannot take is the
 * decoder's refusal to make, as it always was.
 */
const VP9_PROBE = "vp09.00.40.08.01.06.06.06.00";

let chosen: VideoCodec | null = null;

/**
 * Ask the browser once, and remember the answer for `videoCodec()`.
 *
 * Only a definite "no" selects the fallback. An exception — a browser whose
 * `isConfigSupported` rejects a valid string, which has happened — is not a "no",
 * and reads as VP9: the honest failure for a wrong guess is the decoder's own, and
 * VP9 is the codec the rest of the fleet decodes.
 */
export async function chooseVideoCodec(): Promise<VideoCodec> {
  if (chosen) {
    return chosen;
  }
  let supported = true;
  try {
    const answer = await VideoDecoder.isConfigSupported({ codec: VP9_PROBE });
    supported = answer.supported !== false;
  } catch {
    supported = true;
  }
  chosen = supported ? "vp9" : "h264";
  return chosen;
}

/**
 * The codec this browser asked for. Only valid after `chooseVideoCodec` has
 * resolved, which `main.tsx` awaits before mounting; asking earlier is a programming
 * error, not a case to have a default for.
 */
export function videoCodec(): VideoCodec {
  if (!chosen) {
    throw new Error("videoCodec() before chooseVideoCodec() resolved");
  }
  return chosen;
}

/** Test seam: forget the answer so the question can be asked again. */
export function resetVideoCodecForTests(): void {
  chosen = null;
}
