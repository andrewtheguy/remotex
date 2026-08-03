// Pure scheduling policy for decoded remote-audio buffers. It maintains a start
// cushion and discards excess lead rather than accumulating latency.
//
// The two numbers below are the jitter budget, and they were raised roughly fivefold
// after comparing this against every remote-desktop client that does not stutter.
// Myrtille coalesces six ~186 ms blocks (or 1000 ms, whichever comes first) before it
// sends anything, and invalidates at 2000 ms; FreeRDP and Remmina hand sound to
// PulseAudio's default buffer, which is hundreds of milliseconds, behind an *unbounded*
// queue. All of them buy continuity with latency, and all of them discard on elapsed
// time rather than on a queue depth.
//
// The cost is real and is not hidden: audio trails video by roughly the start cushion,
// and nothing here corrects for that. Myrtille accepts the same skew at about a second.

/// Furthest ahead of the audio clock the schedule may run.
export const MAX_LEAD_S = 1.5;

/// Cushion used for a first buffer or recovery from underrun.
export const START_LEAD_S = 0.5;

export interface Scheduled {
  /** When to start this buffer, on the audio context's clock. */
  startAt: number;
  /**
   * Seconds to skip from the *front* of this buffer, which is how catching up is
   * expressed: `AudioBufferSourceNode.start(when, offset)` takes it directly, so
   * nothing is copied and nothing is resampled to drop it.
   */
  trim: number;
  /** Where the timeline stands once this buffer has played. */
  nextAt: number;
  /**
   * The ceiling was hit, so audio already scheduled past `startAt` has to be
   * stopped there. Without that, this buffer would play *over* the tail of the last
   * one rather than in place of it — which is what Guacamole's clamp does, and why
   * it needs a quietest-point search to hide the seam.
   */
  clamped: boolean;
}

/** Place a decoded buffer back-to-back, after an underrun cushion, or at the
 * maximum lead with its excess front trimmed. */
export function scheduleBuffer(
  nextAt: number,
  now: number,
  duration: number,
): Scheduled {
  const startAt = Math.max(nextAt, now + START_LEAD_S);
  const ceiling = now + MAX_LEAD_S;
  if (startAt <= ceiling) {
    return { startAt, trim: 0, nextAt: startAt + duration, clamped: false };
  }
  // Never more than the buffer holds: past that there is nothing left to skip, and
  // the buffer is dropped whole rather than started before it exists.
  const trim = Math.min(startAt - ceiling, duration);
  return {
    startAt: ceiling,
    trim,
    nextAt: ceiling + (duration - trim),
    clamped: true,
  };
}
