// The remote's sound, decoded and scheduled by this browser.
//
// Replaces an `<audio>` element pointed at a live Ogg/Opus response, and the reason
// is latency rather than tidiness: a media element gives no way to shed a delay it
// has accumulated — it resumes where it stopped and never skips forward — so a
// hiccup at any point stayed in the session for the rest of it. Here the schedule is
// ours (see audioSchedule.ts, which is that argument as arithmetic), following
// Guacamole's RawAudioPlayer.
//
// Three pieces, and each is doing the least it can:
//
// - **WebCodecs** decodes the Opus packets. Bare packets arrive on the desktop
//   WebSocket as audio frames; there is no container, because a container exists to
//   delimit and describe packets and the socket already delimits them while
//   `audioFormat` describes them.
// - **Web Audio** plays each decoded buffer at a time we choose, which is the whole
//   point: `source.start(when, offset)` takes both the moment and how much of the
//   front to skip, so catching up costs no copying.
// - **the AudioContext is created inside the click** that enables audio, which is
//   what an autoplay policy wants. The old element had to argue for `autoPlay` and
//   report a refusal; a context resumed from a gesture simply plays.
//
// **Opus only, with no fallback in either direction.** If `AudioDecoder` will not
// take Opus here, this reports that and plays nothing — there is no raw-PCM path to
// fall back to and the WAV one that Opus replaced is gone. That makes browser
// support something to test (`cargo test --lib serve_a_test_tone -- --ignored`)
// rather than something to hedge.

import { type Scheduled, scheduleBuffer } from "./audioSchedule.ts";

/** What `audioFormat` said, which is what a decoder has to be configured with. */
export interface AudioFormat {
  codec: string;
  sampleRate: number;
  channels: number;
  /** `OpusHead`, verbatim: WebCodecs takes it as the config's `description`. */
  head: Uint8Array;
}

export interface AudioPlayer {
  /** One audio frame's Opus packets, in arrival order. */
  push(packets: Uint8Array[]): void;
  /** Stop playing and release the decoder and the context. */
  close(): void;
}

/**
 * Nominal length of one Opus packet, in microseconds.
 *
 * A decoder needs *increasing* timestamps on its input and derives nothing else
 * from them, so this is a label rather than a measurement — but it is the honest
 * label: the gateway cuts 20 ms frames (`FRAME_FRAMES` at 48 kHz in
 * src/opus_stream.rs) and every packet on this wire is one.
 */
const PACKET_US = 20_000;

/** Whether this browser can decode what the gateway sends. */
export async function canPlayOpus(format: AudioFormat): Promise<boolean> {
  if (typeof AudioDecoder === "undefined") {
    return false;
  }
  try {
    const support = await AudioDecoder.isConfigSupported(decoderConfig(format));
    return support.supported === true;
  } catch {
    // A config this browser cannot even parse is an unsupported one.
    return false;
  }
}

function decoderConfig(format: AudioFormat): AudioDecoderConfig {
  return {
    codec: format.codec,
    sampleRate: format.sampleRate,
    numberOfChannels: format.channels,
    // Without this a decoder has to assume a channel count and a pre-skip. The
    // pre-skip is the encoder's own delay, and playing it is playing silence the
    // stream was never meant to contain.
    description: format.head,
  };
}

/**
 * Start playing, and keep the schedule under the ceiling.
 *
 * `onLead` is called with the current lead in seconds on every scheduled buffer —
 * the number that answers the open question in docs/remote-audio.md. If it sits at
 * the ceiling with trims recurring, the delay was arriving as buffered audio and
 * this sheds it; if it hovers at the cushion and the sound is *still* late, the
 * remaining delay is upstream of the gateway.
 */
export function createAudioPlayer(
  format: AudioFormat,
  onLead?: (lead: number, trimmed: number) => void,
): AudioPlayer {
  // The stream's rate, so the common case needs no resampling at all. A device whose
  // hardware disagrees resamples anyway, which is the browser's business.
  const context = new AudioContext({
    sampleRate: format.sampleRate,
    latencyHint: "interactive",
  });
  // Created inside a click, but Safari can still hand back a suspended context.
  void context.resume();

  let nextAt = 0;
  let timestamp = 0;
  let closed = false;
  // Buffers scheduled but not finished. Needed only for the ceiling: pulling the
  // schedule back means audio is already queued *past* the new start time, and it
  // has to be stopped there or the two overlap and mix. Guacamole lets them overlap
  // and hides the seam by splitting packets at their quietest point; stopping the
  // tail instead means there is no seam to hide.
  let playing: AudioBufferSourceNode[] = [];

  const decoder = new AudioDecoder({
    output: (data) => {
      try {
        schedule(data);
      } finally {
        data.close();
      }
    },
    // Nothing is recoverable here: a decoder that has failed will not decode the
    // next packet either, and there is no second representation to switch to.
    error: (e) => {
      console.error("audio: the decoder failed", e);
      close();
    },
  });
  decoder.configure(decoderConfig(format));

  function schedule(data: AudioData): void {
    if (closed || data.numberOfFrames === 0) {
      return;
    }
    const buffer = toAudioBuffer(context, data);
    const at: Scheduled = scheduleBuffer(
      nextAt,
      context.currentTime,
      buffer.duration,
    );
    if (at.clamped) {
      // Everything already scheduled beyond the ceiling gives way to this buffer.
      for (const source of playing) {
        source.stop(at.startAt);
      }
    }
    nextAt = at.nextAt;
    onLead?.(at.nextAt - context.currentTime, at.trim);
    if (at.trim >= buffer.duration) {
      return; // nothing of it is still worth playing
    }

    const source = context.createBufferSource();
    source.buffer = buffer;
    source.connect(context.destination);
    source.onended = () => {
      playing = playing.filter((held) => held !== source);
    };
    playing.push(source);
    // The offset *is* the catch-up: skipping the front of a buffer needs no copy and
    // no resample, only a different argument.
    source.start(at.startAt, at.trim);
  }

  function close(): void {
    if (closed) {
      return;
    }
    closed = true;
    for (const source of playing) {
      source.stop();
    }
    playing = [];
    if (decoder.state !== "closed") {
      decoder.close();
    }
    void context.close();
  }

  return {
    push(packets) {
      if (closed || decoder.state !== "configured") {
        return;
      }
      for (const packet of packets) {
        // Every Opus packet is independently decodable, so they are all key frames —
        // which is also why a listener can attach mid-stream at all.
        decoder.decode(
          new EncodedAudioChunk({
            type: "key",
            timestamp,
            data: packet,
          }),
        );
        timestamp += PACKET_US;
      }
    },
    close,
  };
}

/**
 * A decoded frame as something Web Audio can play.
 *
 * Planar `f32` rather than whatever the decoder happens to hold: `copyTo` converts,
 * and asking for one layout means this does not quietly depend on a browser's
 * internal choice. The channels are copied one at a time for the same reason the
 * gateway's resampler works on deinterleaved data — anything that treats interleaved
 * samples as one signal blends left into right.
 */
function toAudioBuffer(context: AudioContext, data: AudioData): AudioBuffer {
  const frames = data.numberOfFrames;
  const buffer = context.createBuffer(
    data.numberOfChannels,
    frames,
    data.sampleRate,
  );
  const plane = new Float32Array(frames);
  for (let channel = 0; channel < data.numberOfChannels; channel++) {
    data.copyTo(plane, { planeIndex: channel, format: "f32-planar" });
    buffer.copyToChannel(plane, channel);
  }
  return buffer;
}
