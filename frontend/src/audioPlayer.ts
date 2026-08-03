// Browser-owned remote audio: a packet becomes an `AudioBuffer`, and Web Audio
// schedules it with bounded lead. There are two ways to get from one to the
// other and `audioFormat` says which — WebCodecs decodes an encoded packet, and
// a passthrough packet is already samples. The scheduling half below is the same
// either way and is where the interesting behaviour is.
//
// AudioContext creation stays in the enabling click. WebCodecs is required only
// for the encoded path, which is also the only one that needs a secure context.

import { type Scheduled, scheduleBuffer } from "./audioSchedule.ts";

/** What `audioFormat` said, which is everything needed to play the packets. */
export interface AudioFormat {
  /** `"opus"` — a WebCodecs codec string — or {@link PCM_CODEC}. */
  codec: string;
  /**
   * The rate the packets are at: 48 kHz for Opus, because that is what the
   * gateway resampled to, and the remote's own rate under {@link PCM_CODEC},
   * because nothing resampled it. An `AudioBuffer` carries its own rate, so a
   * context at a different one simply resamples on playback.
   */
  sampleRate: number;
  channels: number;
  /**
   * Samples in one packet at `sampleRate`: 960 on Opus, and 0 under
   * {@link PCM_CODEC}, whose packets are whatever length the remote's buffers
   * were and so carry their own.
   */
  packetFrames: number;
  /**
   * `OpusHead`, verbatim: WebCodecs takes it as the config's `description`.
   * Empty under {@link PCM_CODEC}, where there is no decoder to configure.
   */
  head: Uint8Array;
}

/**
 * The gateway is sending the remote's PCM as it arrived, not an encoded stream.
 *
 * Deliberately not a WebCodecs codec string, because there is no WebCodecs
 * decoder for PCM and this path does not want one: the packets are interleaved
 * signed 16-bit little-endian samples at `sampleRate`, which is what an
 * `AudioBuffer` holds already. The name spells the sample format out because
 * `audioFormat`'s other fields do not carry it.
 */
export const PCM_CODEC = "pcm-s16le";

/**
 * Whether this format has to go through WebCodecs, which is what decides
 * whether {@link audioUnavailable} applies to it at all.
 */
export function needsDecoder(format: AudioFormat): boolean {
  return format.codec !== PCM_CODEC;
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
  /** One audio frame's encoded packets, in arrival order. */
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
 * The rate the context is built at, before anything is known about the stream.
 *
 * A guess, and it has to be one: the context must exist inside the enabling
 * click, and `audioFormat` arrives a round trip later. 48 kHz is the rate the
 * gateway encodes at (src/pcm48.rs) and what most output hardware runs at, so
 * the common case resamples nothing. A passthrough stream at 44.1 kHz does get
 * resampled here — by Web Audio, on playback, exactly as it would be by the OS
 * mixer for any buffer whose rate is not the device's. Nothing about the samples
 * that crossed the network changed.
 */
const STREAM_RATE = 48_000;

/**
 * Nominal length of one packet, in microseconds, from what the gateway said.
 *
 * A decoder needs *increasing* timestamps on its input and derives nothing else
 * from them, so this is a label rather than a measurement — but it is the honest
 * label. Only the encoded path uses it; passthrough never reaches a decoder.
 */
function packetDurationUs(format: AudioFormat): number {
  return Math.round((format.packetFrames / format.sampleRate) * 1_000_000);
}

/**
 * Why a *decoder* cannot be had here, distinguishing insecure origin from no
 * decoder at all.
 *
 * Only asked about a format that needs one (see {@link needsDecoder}): a
 * passthrough stream plays without WebCodecs, and so plays on origins where
 * WebCodecs does not exist.
 */
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
    sampleRate: STREAM_RATE,
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
   * browser will not decode what the target's codec produces. There is no
   * fallback to switch to — the codec is the gateway's to choose — so this is
   * reported rather than worked around.
   */
  onError: (reason: string) => void;
  /** Current scheduling lead and seconds trimmed from this buffer. */
  onLead?: (lead: number, trimmed: number) => void;
}

/**
 * Start playing on `context`, keeping the schedule under the ceiling.
 *
 * Throws if the format needs an `AudioDecoder` and there is none to be had (see
 * {@link audioUnavailable}); an *unsupported codec* is not a throw, because
 * WebCodecs reports that asynchronously — it arrives at `onError`.
 */
export function createAudioPlayer(
  format: AudioFormat,
  context: AudioContext,
  handlers: AudioHandlers,
): AudioPlayer {
  const packetUs = packetDurationUs(format);
  let nextAt = 0;
  let timestamp = 0;
  let closed = false;
  // Buffers scheduled past a lead clamp must be stopped to prevent overlap.
  let playing: AudioBufferSourceNode[] = [];

  // Null on the passthrough path, where a packet is already samples. That is the
  // whole of the difference: everything below `schedule` is shared, because a
  // buffer is a buffer however it was arrived at.
  const decoder = needsDecoder(format) ? createDecoder() : null;

  function createDecoder(): AudioDecoder {
    const unavailable = audioUnavailable();
    if (unavailable) {
      throw new Error(unavailable);
    }
    const built = new AudioDecoder({
      output: (data) => {
        try {
          // Checked before the buffer is built, not after: `createBuffer`
          // throws on a zero-length buffer rather than returning an empty one.
          if (data.numberOfFrames > 0) {
            schedule(toAudioBuffer(context, data));
          }
        } finally {
          data.close();
        }
      },
      // Nothing is recoverable here: a decoder that has failed will not decode the
      // next packet either, and there is no second representation to switch to. This
      // is also where "this browser cannot decode this codec" lands — `configure`
      // accepts an unsupported codec and fails asynchronously — so the message names
      // the codec, which is the thing worth putting in a bug report.
      error: (e) => {
        console.error("audio: the decoder failed", e);
        close();
        handlers.onError(
          e instanceof Error && e.name === "NotSupportedError"
            ? `This browser cannot decode the ${format.codec} audio the gateway sends.`
            : "This browser's audio decoder failed.",
        );
      },
    });
    built.configure(decoderConfig(format));
    return built;
  }

  function schedule(buffer: AudioBuffer): void {
    if (closed || buffer.length === 0) {
      return;
    }
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
    if (decoder && decoder.state !== "closed") {
      decoder.close();
    }
    void context.close();
  }

  return {
    push(packets) {
      if (closed) {
        return;
      }
      if (!decoder) {
        for (const packet of packets) {
          if (packet.byteLength >= format.channels * 2) {
            schedule(pcmToAudioBuffer(context, packet, format));
          }
        }
        return;
      }
      if (decoder.state !== "configured") {
        return;
      }
      for (const packet of packets) {
        // Every packet on this wire is independently decodable — an Opus packet
        // is — so they are all key frames, which is also why a listener can
        // attach mid-stream at all.
        decoder.decode(
          new EncodedAudioChunk({
            type: "key",
            timestamp,
            data: packet,
          }),
        );
        timestamp += packetUs;
      }
    },
    close,
  };
}

/**
 * One passthrough packet, split into a planar `f32` channel each.
 *
 * The only conversion on this whole path, and it is forced: `AudioBuffer` holds
 * planar `f32` and there is no arrangement in which it does not. `/ 32768` is the
 * exact inverse of the gateway's own scaling (src/pcm48.rs), so `i16::MIN` maps
 * to exactly -1.0 and nothing is clipped.
 *
 * Planar rather than interleaved for the same reason the gateway's resampler
 * works that way: anything that treats interleaved samples as one signal blends
 * left into right.
 *
 * The frame count comes from the packet's own length, because there is nowhere
 * else it could come from — passthrough packets are whatever size the remote's
 * wave buffers were, which is why `packetFrames` is 0. A trailing part-frame is
 * dropped; the gateway holds one back rather than sending it (src/pcm_stream.rs),
 * so this is the floor under a malformed packet rather than a case that happens.
 */
export function pcmChannels(
  packet: Uint8Array,
  channels: number,
  // Backed by an `ArrayBuffer` rather than the wider `ArrayBufferLike`, which is
  // what `copyToChannel` takes: a `SharedArrayBuffer` is not something these are
  // ever built on, and saying so here is what lets the buffer be filled directly.
): Float32Array<ArrayBuffer>[] {
  const frames = Math.floor(packet.byteLength / (channels * 2));
  // A view over the packet's own bytes: `decodeAudioFrame` hands out subarrays,
  // so the offset is not zero and `new DataView(packet.buffer)` would read some
  // other packet in the same frame.
  const view = new DataView(
    packet.buffer,
    packet.byteOffset,
    packet.byteLength,
  );
  const planes: Float32Array<ArrayBuffer>[] = [];
  for (let channel = 0; channel < channels; channel++) {
    const plane = new Float32Array(frames);
    for (let frame = 0; frame < frames; frame++) {
      plane[frame] =
        view.getInt16((frame * channels + channel) * 2, true) / 32_768;
    }
    planes.push(plane);
  }
  return planes;
}

/** One passthrough packet as something Web Audio can play. */
function pcmToAudioBuffer(
  context: AudioContext,
  packet: Uint8Array,
  format: AudioFormat,
): AudioBuffer {
  const planes = pcmChannels(packet, format.channels);
  const buffer = context.createBuffer(
    format.channels,
    // Never zero: `createBuffer` throws on a zero-length buffer, and the caller
    // already skips a packet too short to hold a frame.
    Math.max(planes[0]?.length ?? 0, 1),
    format.sampleRate,
  );
  for (const [channel, plane] of planes.entries()) {
    buffer.copyToChannel(plane, channel);
  }
  return buffer;
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
