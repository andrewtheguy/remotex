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
// **Native `controls`, and that is the point of this panel.** Whether a browser
// plays such a response *progressively* rather than waiting for it to end is the
// question this whole path exists to answer, and the browser's own transport
// controls are where that shows: a stream that is playing, one that is stalled,
// and one that never started look different here and identical behind a custom
// button. They are also the fallback the autoplay policy needs — `autoPlay` is
// honest because this panel is mounted by a click, but a browser may still refuse
// it, and then there is a visible Play to press.
//
// **Closing the panel stops the sound**, because the element goes with it and the
// HTTP response ends when its consumer disconnects. That is a real limitation —
// the panel sits over the bottom of the desktop while it plays — and the trade for
// not portalling one element through two components in a proof of concept.

import { useRef, useState } from "react";
import { useDockedHeight } from "./SoftKeyboardPanel.tsx";

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
  // Set from the element's own `error` event, which is how a 403 or 503 from the
  // endpoint reaches this side — a media element reports a failed load as an
  // error on itself and never as a rejected promise.
  const [failed, setFailed] = useState(false);

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
        {/* No `muted`: this element exists to be heard. No `loop`, no
            `preload` — the response has no length to preload and no beginning to
            return to. */}
        {/* biome-ignore lint/a11y/useMediaCaption: live desktop audio has no caption track to offer */}
        <audio
          className="ap-player"
          src={src}
          controls
          autoPlay
          onPlaying={() => setFailed(false)}
          onError={() => setFailed(true)}
        />
        <p className="ap-note">
          {failed
            ? "The gateway would not stream this session's audio. It ends when the target disconnects or another browser takes the session over."
            : "Live sound from the remote desktop. Closing this panel stops it."}
        </p>
      </div>
    </div>
  );
}
