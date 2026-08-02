// Remote audio, over the same WebCodecs path the browser SPA uses.
//
// The app owns the *subscription* — the Remote menu's toggle is what puts
// `{"type":"audio"}` on the wire — and this page owns decode and playback. It
// reports back only what the menu cannot know from the wire: whether sound is
// actually coming out, and why not when it isn't.
//
// There is no enabling gesture here, unlike in a browser tab. The app sets
// `mediaTypesRequiringUserActionForPlayback` to nothing, so the context can be
// built when the format arrives rather than inside a click.

import {
  type AudioPlayer,
  audioUnavailable,
  createAudioContext,
  createAudioPlayer,
  decodeAudioHead,
} from "../audioPlayer.ts";
import { decodeAudioFrame } from "../protocol.ts";

export interface ViewerAudio {
  start(format: {
    codec: string;
    sampleRate: number;
    channels: number;
    head: string;
  }): void;
  /** One binary audio frame, verbatim off the gateway's socket. */
  play(frame: ArrayBuffer): void;
  stop(): void;
}

export function createViewerAudio(handlers: {
  onState: (playing: boolean, error: string | null) => void;
}): ViewerAudio {
  let context: AudioContext | null = null;
  let player: AudioPlayer | null = null;

  // Exactly one of the two holds the context, which is what keeps this from
  // closing an already-closed one.
  const release = () => {
    player?.close();
    player = null;
    void context?.close();
    context = null;
  };

  const fail = (reason: string) => {
    release();
    handlers.onState(false, reason);
  };

  return {
    start(format) {
      release();
      const unavailable = audioUnavailable();
      if (unavailable) {
        handlers.onState(false, unavailable);
        return;
      }
      try {
        // Held here before the player is built, not in a local: if
        // `createAudioPlayer` throws, `fail` -> `release` is the only thing that
        // will ever close this context, and it can only close what it can see.
        // A leaked one holds the audio hardware for the life of the page.
        context = createAudioContext();
        player = createAudioPlayer(
          {
            codec: format.codec,
            sampleRate: format.sampleRate,
            channels: format.channels,
            head: decodeAudioHead(format.head),
          },
          context,
          {
            onError: (reason) => fail(reason),
            onLead: (lead, trimmed) => {
              // Only the trims, and they earn a warning: audio was thrown away
              // to stay near live. A steady-state lead cannot leave the range
              // the schedule defines, so it is not worth a line.
              if (trimmed > 0) {
                console.warn(
                  `audio: trimmed ${trimmed.toFixed(3)}s to stay near live (lead ${lead.toFixed(3)}s)`,
                );
              }
            },
          },
        );
        // The player owns the context now, so `release` must not also close it
        // — it closes the player, which closes the context.
        context = null;
        handlers.onState(true, null);
      } catch (cause) {
        fail(
          cause instanceof Error
            ? cause.message
            : "this build cannot play remote audio",
        );
      }
    },
    play(frame) {
      const packets = decodeAudioFrame(frame);
      if (packets) {
        player?.push(packets);
      }
    },
    stop() {
      release();
      handlers.onState(false, null);
    },
  };
}
