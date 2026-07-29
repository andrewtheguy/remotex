// Browser-owned remote audio: WebCodecs decodes bare Opus packets and Web Audio
// schedules them with bounded lead. AudioContext creation stays in the enabling
// click, and non-local origins require HTTPS for WebCodecs.

import { type Scheduled, scheduleBuffer } from "./audioSchedule.ts";

/** What `audioFormat` said, which is what a decoder has to be configured with. */
export interface AudioFormat {
  codec: string;
  sampleRate: number;
  channels: number;
  /** `OpusHead`, verbatim: WebCodecs takes it as the config's `description`. */
  head: Uint8Array;
}

/**
 * The `head` field of an `audioFormat` message as the bytes a decoder wants.
 *
 * base64 because a text frame cannot carry bytes — the same reason the cursor's PNG
 * is base64 — and 19 bytes once a session is not worth a second binary frame kind.
 */
export function decodeAudioHead(head: string): Uint8Array {
  const binary = atob(head);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

export interface AudioPlayer {
  /** One audio frame's Opus packets, in arrival order. */
  push(packets: Uint8Array[]): void;
  /**
   * Stop playing, and release the decoder **and the context** — the player takes
   * ownership of the context it was handed, so a caller needs one call rather than
   * two and cannot leave the audio hardware held open. Getting sound back means a
   * fresh context, which is no imposition: that only happens on a click.
   */
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

/**
 * The rate everything here runs at.
 *
 * Not a preference and not negotiable: libopus encodes at 48 kHz and nothing else,
 * so this is the rate the gateway's stream is in whatever the remote negotiated
 * (see src/opus_stream.rs). Naming it here lets the context be built before the
 * `audioFormat` message arrives, which is the whole trick below.
 */
const OPUS_RATE = 48_000;

/** Why audio cannot play here, distinguishing insecure origin from no decoder. */
export function audioUnavailable(): string | null {
  if (typeof AudioDecoder !== "undefined") {
    return null;
  }
  if (!window.isSecureContext) {
    return "Audio needs a secure context: reach this gateway over HTTPS (or localhost).";
  }
  return "This browser has no WebCodecs audio decoder.";
}

/**
 * The audio context, built **inside the click** that enables audio.
 *
 * Separate from the player because of *when* rather than what: the format needed to
 * configure a decoder arrives a round trip later, and by then the gesture is over.
 * Safari will hand back a suspended context and refuse to resume one outside a user
 * gesture, so the context has to be created here and the decoder wrapped around it
 * when the format lands.
 */
export function createAudioContext(): AudioContext {
  const context = new AudioContext({
    // The stream's own rate, so the common case needs no resampling at all. A
    // device whose hardware disagrees resamples anyway, which is its business.
    sampleRate: OPUS_RATE,
    latencyHint: "interactive",
  });
  void context.resume();
  return context;
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

export interface AudioHandlers {
  /**
   * The decoder gave up, which on this path means one thing in practice: this
   * browser will not decode Opus. There is no fallback to switch to, so this is
   * reported rather than worked around.
   */
  onError: (reason: string) => void;
  /** Current scheduling lead and seconds trimmed from this buffer. */
  onLead?: (lead: number, trimmed: number) => void;
}

/**
 * Start playing on `context`, keeping the schedule under the ceiling.
 *
 * Throws if there is no `AudioDecoder` to be had (see [`audioUnavailable`]); an
 * *unsupported codec* is not a throw, because WebCodecs reports that
 * asynchronously — it arrives at `onError`.
 */
export function createAudioPlayer(
  format: AudioFormat,
  context: AudioContext,
  handlers: AudioHandlers,
): AudioPlayer {
  const unavailable = audioUnavailable();
  if (unavailable) {
    throw new Error(unavailable);
  }
  let nextAt = 0;
  let timestamp = 0;
  let closed = false;
  // Buffers scheduled past a lead clamp must be stopped to prevent overlap.
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
    // next packet either, and there is no second representation to switch to. This
    // is also where "this browser cannot decode Opus" lands — `configure` accepts an
    // unsupported codec and fails asynchronously.
    error: (e) => {
      console.error("audio: the decoder failed", e);
      close();
      handlers.onError(
        e instanceof Error && e.name === "NotSupportedError"
          ? "This browser cannot decode the Opus audio the gateway sends."
          : "This browser's audio decoder failed.",
      );
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
    handlers.onLead?.(at.nextAt - context.currentTime, at.trim);
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
