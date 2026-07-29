// When each buffer of remote audio should play, and how much of it to throw away.
//
// Split out from audioPlayer.ts because it is the whole idea of this change
// expressed as arithmetic, and arithmetic can be tested without a browser. The
// player around it does the parts that cannot: an AudioContext, a WebCodecs
// decoder, and the Web Audio nodes.
//
// **The point: the client owns the schedule.** The gateway used to serve audio as
// an open-ended Ogg/Opus response and an `<audio>` element played it, which sounds
// simpler and gives away the one thing that matters — a media element resumes where
// it stopped and *never skips forward*, so whatever it fell behind by during
// start-up buffering or one hiccup, it stayed behind by. Both of the gateway's old
// latency devices existed because of that (a keepalive trickling silence below real
// time so a listener could drain back toward live; a `playbackRate` nudge that was
// never once observed to engage), and neither could bound the delay.
//
// Apache Guacamole's RawAudioPlayer bounds it in one line, in `sync()`:
//
//   nextPacketTime = Math.min(nextPacketTime, now + maxLatency);   // 0.3
//
// This is that, with two deliberate differences — see the constants.

/// The furthest ahead of the clock the schedule may run.
///
/// Guacamole's `maxLatency`, and the same number: a third of a second is comfortably
/// more than the ~186 ms wave buffer the tested Windows host sends, so a link
/// delivering at real time never reaches it, and a burst cannot bank more than one
/// buffer's worth of delay.
export const MAX_LEAD_S = 0.3;

/// How far ahead a fresh start — or a recovery from an underrun — is scheduled.
///
/// Guacamole has no equivalent: it schedules at `max(now, nextPacketTime)`, so a
/// stream that has just started, or has just run dry, is playing with **zero**
/// cushion and the very next jitter is another gap. The host paces itself to real
/// time but not to a clock — measured inter-arrival ran 169–200 ms around a 186 ms
/// buffer — so a cushion in that range costs a tenth of a second of latency and buys
/// out the ordinary case.
export const START_LEAD_S = 0.1;

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

/**
 * Place one decoded buffer on the timeline.
 *
 * `nextAt` is where the previous buffer left the timeline (`0` before anything has
 * played), `now` is `AudioContext.currentTime`, and `duration` is the buffer's own
 * length in seconds.
 *
 * Three cases, and the first two are Guacamole's:
 *
 * - **Back to back.** The ordinary case: this buffer follows the last one exactly,
 *   with no gap to hear and no overlap to muddy.
 * - **Behind the clock.** The schedule has run dry — a first buffer, or a gap while
 *   the remote was quiet — so it restarts at `now + START_LEAD_S`. Anything earlier
 *   than `now` cannot play at all, and anything without the cushion is one jitter
 *   from another underrun.
 * - **Too far ahead.** More audio has arrived than real time can absorb. The
 *   schedule is pulled back to the ceiling and the front of this buffer is thrown
 *   away, so the *delay* is discarded rather than the sound being played late.
 *   Skipping forward to stay near live is the same choice the gateway's queue
 *   already makes when a consumer falls behind.
 */
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
