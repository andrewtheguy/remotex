// Whether this browser decodes a High Performance Mac's own HEVC: the second question
// about its decoders the gateway is told, asked once, before the client mounts, beside
// the chroma (videoChroma.ts).
//
// A target with `hevc_passthrough` passes the Mac's stream to a browser that says yes
// instead of decoding it and encoding VP9, and a browser that says no is sent VP9 as
// from any other target. The stream is HEVC Range Extensions, 4:4:4, which Chrome and
// Safari decode on the platforms measured and Firefox does not, so the browser has to
// say. The answer rides every session socket this page opens (`gateway.ts`), for the
// same reason the chroma does.
//
// Selection, never refusal, as with the chroma — but the other way round on a doubt.
// VP9 is what every browser here decodes, so only a definite "yes" asks for the Mac's
// stream, and an exception from `isConfigSupported` reads as "no".

/**
 * The configuration asked about: macwork's stream, 1600×1000, as its sequence
 * parameter set names it (`parse_sps` in src/vnc_apple_media.rs) — Range Extensions,
 * level 5.0, the 4:4:4 constraint flags.
 */
const APPLE_HEVC_PROBE = "hev1.4.10.L150.BE.8";

let answer: boolean | null = null;

/** Ask the browser once, and remember the answer for `decodesAppleHevc()`. */
export async function chooseAppleHevc(): Promise<boolean> {
  if (answer !== null) {
    return answer;
  }
  try {
    const support = await VideoDecoder.isConfigSupported({
      codec: APPLE_HEVC_PROBE,
    });
    answer = support.supported === true;
  } catch {
    answer = false;
  }
  return answer;
}

/**
 * The answer. Only valid after `chooseAppleHevc` has resolved, which `main.tsx`
 * awaits before mounting.
 */
export function decodesAppleHevc(): boolean {
  if (answer === null) {
    throw new Error("decodesAppleHevc() before chooseAppleHevc() resolved");
  }
  return answer;
}

/** Test seam: forget the answer so the question can be asked again. */
export function resetAppleHevcForTests(): void {
  answer = null;
}
