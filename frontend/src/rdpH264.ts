// Whether this page decodes the H.264 an RDP host may draw with on a passed
// pipeline: the third question about itself the gateway is told, beside the Mac's
// stream (appleMedia.ts) and the pipeline itself (rdpGraphics.ts).
//
// EXPERIMENTAL, and behind a target's `egfx_h264` key. A host told its client takes
// H.264 hands the parts of the desktop that move like video to it. The gateway has
// no decoder for it and passes the access units on inside the pipeline's commands;
// this page decodes them with the browser's `VideoDecoder`, a stream for each
// surface (egfxVideo.ts), and has each picture's samples copied into the
// compositor's memory, where they are put into colour (egfxCompositor.ts).
//
// So the answer is what a session will ask of the browser, asked ahead of one: a
// short stream shaped like a Windows host's goes through a session's own decoders,
// a unit at a time, and each unit has to give its picture before the next is
// handed over, in a layout the compositor reads, and copy into memory that is
// shared, which the compositor's is. A decoder that holds a picture back for the
// units after it gives none in time, and that is a no: a pipeline is composed in
// command order and cannot wait on what the host has not sent yet.
//
// It rides every session socket this page opens (`gateway.ts`). Unlike the others
// it turns no session away and greys no choice: a page that says no is passed a
// pipeline the host was told to keep H.264 out of.

import { createEgfxVideo } from "./egfxVideo.ts";

/**
 * The stream asked about: what a Windows 11 host sent at 1280×800, as its sequence
 * parameter set names it — Main profile, level 3.2.
 */
const RDP_H264_PROBE = "avc1.4d4020";

/**
 * Three access units of such a stream, a keyframe and the two after it, base64:
 * 1280×800 of one grey, since the size is part of what a browser picks a decoder
 * by. Its parameter sets say what the host's do — Annex B, an access unit
 * delimiter at each unit, the parameter sets inline at the keyframe, one reference
 * picture, `pic_order_cnt_type` 2 and `max_num_reorder_frames` 0 — so nothing in
 * it gives a decoder a reason to wait. Made with x264:
 *
 *     ffmpeg -f lavfi -i color=c=0x808080:s=1280x800:r=30 -frames:v 3 \
 *       -c:v libx264 -profile:v main -pix_fmt yuv420p -qp 30 -threads 1 \
 *       -x264-params bframes=0:ref=1:aud=1:keyint=infinite:scenecut=0:fullrange=on \
 *       -bsf:v filter_units=remove_types=6 -f h264 probe.h264
 */
const PROBE_UNITS = [
  "AAAAAQkQAAAAAWdNQCDaAUAZbAWyAAADAAIAAAMAeB4wZUAAAAABaO8ESyAAAAFliIQ//rXO7pN5" +
    "hVZYdk4Y770JoAAAAwAAAwAAAwAAD5kEVkCMT23GgAAAAwAAuIABQwAC1gAI+AAdwACEAALIABHA" +
    "AF8AAvAAEQAAiAAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAA" +
    "AwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAA" +
    "AwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwID",
  "AAAAAQkwAAABQZomPwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAwIA==",
  "AAAAAQkwAAABQZpGPwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAAAwAwIQ==",
];

/** The side of the corner of each picture that is copied: one macroblock. */
const PROBE_COPY = 16;

/**
 * How long a decoder has for each unit's picture here. Shorter than a session
 * gives one (egfxVideo.ts): the page waits on this answer before it mounts, and a
 * decoder too slow for it is only passed a pipeline with no H.264 in it.
 */
let patienceMs = 2000;

let answer: boolean | null = null;

/**
 * A corner of a decoded picture copied into shared memory, as the compositor is
 * handed a unit's window (`supply` in egfxCompositor.ts). Throws where it cannot
 * be, or where the picture is laid out in a way the compositor does not read.
 */
async function copyIntoSharedMemory(frame: VideoFrame): Promise<void> {
  if (frame.format !== "I420" && frame.format !== "NV12") {
    throw new Error(`its H.264 decoder's pictures are ${frame.format}`);
  }
  const rect = {
    x: frame.visibleRect?.x ?? 0,
    y: frame.visibleRect?.y ?? 0,
    width: PROBE_COPY,
    height: PROBE_COPY,
  };
  const room = new Uint8Array(
    new SharedArrayBuffer(frame.allocationSize({ rect })),
  );
  await frame.copyTo(room, { rect });
}

/**
 * The stream through a session's decoders, each unit's picture awaited before the
 * next unit is handed over. Throws where the browser has no decoder for it, and
 * at the first unit that gives no picture in time.
 */
async function decodeAUnitAtATime(): Promise<void> {
  const video = createEgfxVideo(patienceMs);
  try {
    for (const [index, unit] of PROBE_UNITS.entries()) {
      const data = Uint8Array.from(atob(unit), (c) => c.charCodeAt(0));
      const key = index === 0;
      const frame = await video.decode(
        {
          surface: 0,
          start: 0,
          end: data.length,
          key,
          codec: key ? RDP_H264_PROBE : null,
          window: "whole",
        },
        data,
      );
      try {
        await copyIntoSharedMemory(frame);
      } finally {
        frame.close();
      }
    }
  } finally {
    video.close();
  }
}

/**
 * Ask the browser once, and remember the answer, which `decodesRdpH264()` then
 * says. Anything that throws reads as no.
 */
export async function chooseRdpH264(): Promise<boolean> {
  if (answer === null) {
    try {
      await decodeAUnitAtATime();
      answer = true;
    } catch {
      answer = false;
    }
  }
  return answer;
}

/**
 * Whether this page decodes a passed pipeline's H.264. Only valid after
 * `chooseRdpH264` has resolved, which `main.tsx` awaits before mounting.
 */
export function decodesRdpH264(): boolean {
  if (answer === null) {
    throw new Error("decodesRdpH264() read before chooseRdpH264() resolved");
  }
  return answer;
}

/**
 * Test seam: forget the answer so the question can be asked again, with the wait
 * a test can afford.
 */
export function resetRdpH264ForTests(timeoutMs = 2000): void {
  answer = null;
  patienceMs = timeoutMs;
}
