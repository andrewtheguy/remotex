// The remote's sound, played by the browser's own audio element.
//
// There is deliberately no audio pipeline here: no WebCodecs, no AudioWorklet,
// no jitter buffer, no MSE. The gateway serves the session's live audio as an
// ordinary open-ended `audio/ogg; codecs=opus` response and this points an
// `<audio>` at it, which leaves buffering, decoding and playback to the browser
// (see docs/remote-audio.md).
//
// Ogg/Opus rather than the raw PCM this started as, purely for bandwidth: ~96 kbps
// against 1.4 Mbit/s. Support for it in `<audio>` is recent enough to be worth
// checking on a device rather than looking up — Safari only gained Ogg/Opus in
// 18.4 — and `server::tests::serve_a_test_tone` is how to check.
//
// **Nothing here waits for the remote to be making a sound.** The endpoint answers
// straight away and sends silence until sound arrives, so opening this panel on a
// quiet desktop gives a player that is playing and will start on its own — and keep
// going when the remote goes quiet again and comes back. That is the reason there is
// one element mounted once rather than a reload on each silence: a re-load is a
// fresh autoplay attempt long after the click that permitted it, which iOS Safari
// may refuse.
//
// **Enable/disable, not the native transport.** This element carries no `controls`,
// and that is a change of mind worth recording: the native controls were the point
// of this panel while the open question was whether a browser plays an open-ended
// response *progressively*, because a stream that is playing, one that is stalled
// and one that never started look different in them. That question is answered, so
// what is left is a control that fits a live stream — and a transport does not. Its
// scrubber and elapsed time describe a recording that can be returned to, when this
// has no beginning; and its Pause does not pause the remote, it only drops the
// listener behind live for the rest of the session, since a media element resumes
// where it stopped and never skips forward.
//
// So the one control is a toggle, and disabling really disables: the element is
// unmounted, which ends the HTTP response, and enabling mounts a new one that starts
// at the live edge. That also makes it the way back if playback ever does fall
// behind. The trade is that in-page volume goes with the native controls, leaving
// the system's own; the panel is for listening to a desktop, not for mixing it.
//
// **Closing the panel stops the sound**, for the same reason: the element goes with
// it and the response ends when its consumer disconnects. That is a real limitation
// — the panel sits over the bottom of the desktop while it plays — and the trade for
// not portalling one element through two components in a proof of concept.

import { useEffect, useRef, useState } from "react";
import { useDockedHeight } from "./SoftKeyboardPanel.tsx";

// Catching up to live, which is the only lever a media element has for its own
// latency: it cannot be told to skip forward, but it can be told to play faster.
//
// Measure the gap between the end of the buffered range and the playhead, and where
// it is more than a fraction of a second, play 8% fast until it is not. Pitch is
// preserved by default, so the effect is inaudible on speech and music alike; the
// hysteresis is wide enough that this engages once and then stops, rather than
// oscillating around a single threshold.
const CATCH_UP_ABOVE_S = 0.4;
const CATCH_UP_UNTIL_S = 0.15;
const CATCH_UP_RATE = 1.08;
const CATCH_UP_EVERY_MS = 500;

// Trim the buffer this element is holding ahead of its playhead.
//
// **Insurance rather than a fix, and it has never been seen to engage.** A live
// desktop was heard a couple of seconds late, and reading this same number on screen
// is what ruled the browser's buffer out: it sat near zero while the sound was late,
// so the element was already at the live edge of what it had been sent and playing
// faster could not have helped. What this still covers is the failure that *was*
// observed, when the gateway kept the stream level with the clock: a standing buffer
// the element will never give back on its own.
function catchUp(player: HTMLAudioElement): void {
  const ranges = player.buffered;
  if (player.paused || ranges.length === 0) {
    return;
  }
  const ahead = ranges.end(ranges.length - 1) - player.currentTime;
  if (ahead > CATCH_UP_ABOVE_S) {
    player.playbackRate = CATCH_UP_RATE;
  } else if (ahead < CATCH_UP_UNTIL_S) {
    player.playbackRate = 1;
  }
}

interface Props {
  /** The session's audio endpoint, carrying its claim token. */
  src: string;
  onClose: () => void;
  onDockedHeightChange?: (px: number) => void;
}

export default function AudioPanel({
  src,
  onClose,
  onDockedHeightChange,
}: Props) {
  const panelRef = useRef<HTMLDivElement>(null);
  useDockedHeight(panelRef, onDockedHeightChange);
  const playerRef = useRef<HTMLAudioElement>(null);
  // Enabled the moment the panel opens. The click that opened it is what permits
  // playback, so spending that click on a second button would be the worse default.
  const [enabled, setEnabled] = useState(true);
  // Set from the element's own `error` event, which is how a refusal from the
  // endpoint reaches this side — a media element reports a failed load as an error
  // on itself and never as a rejected promise. A quiet remote is not one of those
  // refusals any more; what is left is a stale claim token (403) and a session with
  // no audio source at all (503).
  const [failed, setFailed] = useState(false);
  // Autoplay refused. Without the native controls there is no Play button of the
  // browser's own to fall back on, so this has to be visible: the toggle below says
  // "Enable audio" again, and pressing it is a fresh gesture.
  const [blocked, setBlocked] = useState(false);

  // `autoPlay` covers the ordinary case; this covers the one it cannot report,
  // since a refused autoplay is silent — no event, no rejection to observe. Calling
  // `play()` ourselves gives us the rejection.
  useEffect(() => {
    if (!enabled) {
      return;
    }
    playerRef.current?.play().catch(() => setBlocked(true));
  }, [enabled]);

  // Trim whatever buffer the element is holding ahead of the playhead — see above.
  useEffect(() => {
    if (!enabled) {
      return;
    }
    const timer = setInterval(() => {
      const player = playerRef.current;
      if (player) {
        catchUp(player);
      }
    }, CATCH_UP_EVERY_MS);
    return () => clearInterval(timer);
  }, [enabled]);

  return (
    <div className="panel" ref={panelRef}>
      <div className="panel-header">
        <span className="panel-title">Remote audio</span>
        <button
          type="button"
          className="panel-close"
          aria-label="Close remote audio"
          onClick={onClose}
        >
          ✕
        </button>
      </div>

      <div className="ap-body">
        <button
          type="button"
          className="toolbar-btn ap-toggle"
          aria-pressed={enabled}
          onClick={() => {
            setFailed(false);
            setBlocked(false);
            setEnabled((on) => !on);
          }}
        >
          {enabled && !blocked ? "Disable audio" : "Enable audio"}
        </button>
        {/* No `muted`: this element exists to be heard. No `loop`, no `preload`,
            no `controls` — the response has no length to preload, no beginning to
            return to, and nothing a transport could usefully do to it. */}
        {enabled && (
          // biome-ignore lint/a11y/useMediaCaption: live desktop audio has no caption track to offer
          <audio
            ref={playerRef}
            src={src}
            autoPlay
            onPlaying={() => {
              setFailed(false);
              setBlocked(false);
            }}
            onError={() => setFailed(true)}
          />
        )}
        <p className="ap-note">
          {failed
            ? "The gateway would not stream this session's audio. It ends when the target disconnects or another browser takes the session over."
            : blocked
              ? "This browser would not start playback on its own. Press Enable audio."
              : enabled
                ? "Live sound from the remote desktop, silent until something plays there. Closing this panel stops it."
                : "Audio is off. Enabling it starts a new stream at whatever the remote is playing now."}
        </p>
      </div>
    </div>
  );
}
