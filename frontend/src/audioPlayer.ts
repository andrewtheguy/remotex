// Browser-owned remote audio: WebCodecs decodes an Opus packet into an
// `AudioBuffer`, and Web Audio schedules it with bounded lead. The scheduling
// half below is where the interesting behaviour is.
//
// AudioContext creation stays in the enabling click wherever the browser needs one
// (AUDIO_NEEDS_GESTURE). That WebCodecs exists at all is
// not a question asked here: it is the client's entry condition (preflight.ts).

import { type Scheduled, scheduleBuffer } from "./audioSchedule.ts";

/** What `audioFormat` said, which is everything needed to play the packets. */
export interface AudioFormat {
  /** `"opus"`, the WebCodecs codec string. */
  codec: string;
  /**
   * The rate the packets are at: 48 kHz, because that is what the gateway
   * resampled to. An `AudioBuffer` carries its own rate, so a context at a
   * different one simply resamples on playback.
   */
  sampleRate: number;
  channels: number;
  /** Samples in one packet at `sampleRate`: 960. */
  packetFrames: number;
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
  /** One audio frame's encoded packets, in arrival order. */
  push(packets: Uint8Array[]): void;
  /**
   * Stop playing, and release the decoder **and the context** — the player takes
   * ownership of the context it was handed, so a caller needs one call rather than
   * two and cannot leave the audio hardware held open. Getting sound back means a
   * fresh context: a click's, or one built for a remembered choice where the
   * browser lets it start without one.
   */
  close(): void;
}

/**
 * The rate the context is built at, before anything is known about the stream.
 *
 * A guess, and it has to be one: the context may have to exist inside the
 * enabling click, and `audioFormat` arrives a round trip later. 48 kHz is the
 * rate the gateway encodes at (src/pcm48.rs) and what most output hardware runs
 * at, so the common case resamples nothing.
 */
const STREAM_RATE = 48_000;

/**
 * How long a splice boundary fade lasts. Long enough to turn a waveform
 * discontinuity's click into nothing, short enough — a fifth of one 20 ms Opus
 * packet — that it is not itself audible as a dip.
 */
const SPLICE_FADE_S = 0.004;

/**
 * Nominal length of one packet, in microseconds, from what the gateway said.
 *
 * A decoder needs *increasing* timestamps on its input and derives nothing else
 * from them, so this is a label rather than a measurement — but it is the honest
 * label.
 */
function packetDurationUs(format: AudioFormat): number {
  return Math.round((format.packetFrames / format.sampleRate) * 1_000_000);
}

/**
 * Whether this browser starts an AudioContext only inside the gesture that creates
 * or resumes it.
 *
 * WebKit does — Safari on every Apple platform, and every browser on iOS and iPadOS,
 * which are all WebKit underneath. A context it builds outside a gesture stays
 * suspended until a `resume()` made inside one, so sound there can never come up on
 * its own after a reload or a reattach: it is always a click on Audio. Chromium and
 * Firefox let a context start once the page has seen any interaction at all, and
 * sooner where their autoplay policy already allows it, so there a remembered
 * choice can be honoured without a click of its own (see
 * {@link createAudioContext}).
 *
 * `GestureEvent` is WebKit's alone, which is what makes it the test: no other engine
 * defines it, and every WebKit does.
 */
export const AUDIO_NEEDS_GESTURE = "GestureEvent" in globalThis;

/**
 * The interactions that grant a page user activation — the moment a suspended
 * context is allowed to start in a browser that is not {@link AUDIO_NEEDS_GESTURE}.
 */
const ACTIVATION_EVENTS = ["pointerdown", "pointerup", "keydown", "touchend"];

/**
 * The audio context, built inside the click that enables audio wherever there is
 * one.
 *
 * Separate from the player because of *when* rather than what: the format needed to
 * configure a decoder arrives a round trip later, and by then the gesture is over.
 * Safari will hand back a suspended context and refuse to resume one outside a user
 * gesture, so the context has to be created here and the decoder wrapped around it
 * when the format lands.
 *
 * A context built with no gesture — a remembered choice reapplied after a reload
 * or a reattach, never in an {@link AUDIO_NEEDS_GESTURE} browser — may come up
 * suspended when the page has not been interacted with yet. It is resumed on the
 * page's next interaction, so the sound that was asked for starts at the first click
 * or key on the desktop instead of waiting for Audio to be toggled again.
 */
export function createAudioContext(): AudioContext {
  const context = new AudioContext({
    // The stream's own rate, so the common case needs no resampling at all. A
    // device whose hardware disagrees resamples anyway, which is its business.
    sampleRate: STREAM_RATE,
    latencyHint: "interactive",
  });
  const resume = () => {
    void context.resume();
  };
  const settle = () => {
    if (context.state === "suspended") {
      return;
    }
    for (const type of ACTIVATION_EVENTS) {
      window.removeEventListener(type, resume, true);
    }
    context.removeEventListener("statechange", settle);
  };
  for (const type of ACTIVATION_EVENTS) {
    window.addEventListener(type, resume, true);
  }
  context.addEventListener("statechange", settle);
  resume();
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
 * Throws if the format is not one a decoder can be configured from — a `head` that
 * is not an `OpusHead`, a channel count nothing can play. An *unsupported codec* is
 * not a throw, because WebCodecs reports that asynchronously: it arrives at
 * `onError`.
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
  // Each keeps its gain node so the stop can be a fade rather than a cut.
  let playing: { source: AudioBufferSourceNode; gain: GainNode }[] = [];

  const decoder = new AudioDecoder({
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
  decoder.configure(decoderConfig(format));

  function schedule(buffer: AudioBuffer): void {
    if (closed || buffer.length === 0) {
      return;
    }
    // Whether this buffer butts seamlessly onto the one before it. Anything
    // else — an underrun restart, the ceiling's trim, the first buffer —
    // starts mid-waveform or after silence, and is faded in over a few
    // milliseconds rather than spliced hard, which is a click.
    const joined = nextAt;
    const at: Scheduled = scheduleBuffer(
      nextAt,
      context.currentTime,
      buffer.duration,
    );
    if (at.clamped) {
      // Everything already scheduled beyond the ceiling gives way to this
      // buffer — faded out into the splice, not cut mid-waveform.
      for (const held of playing) {
        const level = held.gain.gain;
        level.setValueAtTime(
          1,
          Math.max(context.currentTime, at.startAt - SPLICE_FADE_S),
        );
        level.linearRampToValueAtTime(0, at.startAt);
        held.source.stop(at.startAt);
      }
    }
    nextAt = at.nextAt;
    handlers.onLead?.(at.nextAt - context.currentTime, at.trim);
    if (at.trim >= buffer.duration) {
      return; // nothing of it is still worth playing
    }

    const source = context.createBufferSource();
    source.buffer = buffer;
    const gain = context.createGain();
    source.connect(gain);
    gain.connect(context.destination);
    if (at.clamped || at.startAt > joined) {
      gain.gain.setValueAtTime(0, at.startAt);
      gain.gain.linearRampToValueAtTime(1, at.startAt + SPLICE_FADE_S);
    }
    const held = { source, gain };
    source.onended = () => {
      playing = playing.filter((h) => h !== held);
      gain.disconnect();
    };
    playing.push(held);
    // The offset *is* the catch-up: skipping the front of a buffer needs no copy and
    // no resample, only a different argument.
    source.start(at.startAt, at.trim);
  }

  function close(): void {
    if (closed) {
      return;
    }
    closed = true;
    for (const held of playing) {
      held.source.stop();
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
        // Every packet on this wire is independently decodable — an Opus packet
        // is — so they are all key frames, which is also why a listener can
        // attach mid-stream at all.
        decoder.decode(
          new EncodedAudioChunk({ type: "key", timestamp, data: packet }),
        );
        timestamp += packetUs;
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
