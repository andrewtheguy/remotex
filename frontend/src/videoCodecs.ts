// Whether this browser's own `VideoDecoder` takes the two pictures a target sends
// with nothing of this page's to stand in: wlshare's VP9 at 4:2:0 and a High
// Performance Mac's H.264. Asked once, before the client mounts, beside the
// questions about VP9 at 4:4:4 (nativeVp9.ts), the Mac's HEVC (appleMedia.ts) and
// an RDP host's H.264 (rdpH264.ts).
//
// A browser without a `VideoDecoder` at all starts like any other: everything it
// cannot decode is greyed at the picker with the reason, and what it can — the
// gateway's tiles, a Standard Mac's Adaptive picture and an RDP host's pipeline
// without H.264, each decoded by the page — still starts. So these answers say no
// for it, and the picker greys by them (targetChoices.ts): the Mac's H.264, and a
// `wlshare` target, whose VP9 is its picture in every session.
//
// A doubt reads as no, as the other questions read one: a picture this browser's
// decoder then refuses has nothing to fall back on, so only a definite "yes"
// offers it, and anything that throws, a missing `VideoDecoder` among them, reads
// as "no". The gateway is not told: it is sent no choice the picker greys.

/**
 * wlshare's VP9 at 4:2:0, profile 0, level 4.0, eight bits, BT.601 studio swing —
 * a 1080p desktop, in the shape the gateway announces it (`codec_string` in
 * src/vp9.rs). 4:2:0 is what every wlshare session can be sent, so it is the one
 * asked about.
 */
const VP9_PROBE = "vp09.00.40.08.01.06.06.06.00";

/**
 * The Mac's H.264 for a strip of a 1600×1000 display, as its sequence parameter
 * set names it (`StreamParams` in src/vnc_apple_media.rs) — High profile, level
 * 3.2. Strips are what a session is sent, but for a display too low for them.
 */
const APPLE_H264_PROBE = "avc1.640020";

let answers: { vp9: boolean; appleH264: boolean } | null = null;

async function decodes(codec: string): Promise<boolean> {
  try {
    const reply = await VideoDecoder.isConfigSupported({ codec });
    return reply.supported === true;
  } catch {
    return false;
  }
}

/** Ask the browser once, and remember the answers. */
export async function askVideoCodecs(): Promise<void> {
  if (answers !== null) {
    return;
  }
  const [vp9, appleH264] = await Promise.all([
    decodes(VP9_PROBE),
    decodes(APPLE_H264_PROBE),
  ]);
  answers = { vp9, appleH264 };
}

function answered(): { vp9: boolean; appleH264: boolean } {
  if (answers === null) {
    throw new Error("video codecs read before askVideoCodecs() resolved");
  }
  return answers;
}

/**
 * Whether the browser's own decoder takes wlshare's VP9. Only valid after
 * `askVideoCodecs` has resolved, which `main.tsx` awaits before mounting.
 */
export function decodesVp9(): boolean {
  return answered().vp9;
}

/** Whether it takes a High Performance Mac's H.264, on the same terms. */
export function decodesAppleH264(): boolean {
  return answered().appleH264;
}

/** Test seam: forget the answers so the question can be asked again. */
export function resetVideoCodecsForTests(): void {
  answers = null;
}
