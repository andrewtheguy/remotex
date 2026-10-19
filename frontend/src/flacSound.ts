// The page's lossless sound: FLAC, played by the browser's own `AudioDecoder`,
// and whether this browser's decodes it.
//
// A session started with lossless sound is sent FLAC instead of Opus: wlshare's
// own frames passed as they came, or an RDP host's PCM coded by the gateway. A
// packet is one FLAC frame, a stream of its own with no header in front of it,
// and `audioFormat` comes with an empty `head`. WebCodecs takes a FLAC stream's
// header as the decoder's `description`, "fLaC" and a STREAMINFO block, and the
// page writes it from what `audioFormat` says, which is everything such a header
// would: the rate, the channels, a block of `packetFrames`, 16 bits a sample.
//
// Asked once, before the client mounts, beside the other questions about the
// browser's decoders (main.tsx): by a real decode of one of the gateway's frames,
// and not by `isConfigSupported`. Safari 27 says it takes FLAC and then fails
// every frame ("InternalAudioDecoderCocoa decoding failed"), where Chrome and
// Firefox decode them exactly. A browser that does not decode the frame is not
// offered lossless sound: the picker greys FLAC with the reason
// (targetChoices.ts), and the gateway is sent no choice the picker greys. A doubt
// reads as no, as the other questions read one: Opus is what such a browser is
// offered instead.

/**
 * What `audioFormat` names lossless sound as, and the codec string WebCodecs
 * decodes it under.
 */
export const FLAC_CODEC = "flac";

/** The bits of every sample: the one width either source has. */
const SAMPLE_BITS = 16;

/** What `audioFormat` says of a FLAC stream: the shape of every frame. */
export interface FlacStream {
  sampleRate: number;
  channels: number;
  /** Samples of one channel in every frame. */
  packetFrames: number;
}

/**
 * The stream's header, as WebCodecs takes a FLAC decoder's `description`: "fLaC",
 * then one metadata block, the last, a STREAMINFO of 34 bytes. Every frame is a
 * block of `packetFrames`; the frames' sizes, the stream's length and its MD5 are
 * not known, and are written as zeros, which the format says they may be.
 */
export function flacHeader(stream: FlacStream): Uint8Array {
  const header = new Uint8Array(4 + 4 + 34);
  header.set([0x66, 0x4c, 0x61, 0x43, 0x80, 0, 0, 34]);
  const info = new DataView(header.buffer, 8);
  info.setUint16(0, stream.packetFrames);
  info.setUint16(2, stream.packetFrames);
  // The rate in 20 bits, the channels less one in 3, the bits less one in 5,
  // and 36 bits of samples in all, unknown.
  info.setBigUint64(
    10,
    (BigInt(stream.sampleRate) << 44n) |
      (BigInt(stream.channels - 1) << 41n) |
      (BigInt(SAMPLE_BITS - 1) << 36n),
  );
  return header;
}

/** The decoder's configuration for a FLAC stream. */
export function flacConfig(stream: FlacStream): AudioDecoderConfig {
  return {
    codec: FLAC_CODEC,
    sampleRate: stream.sampleRate,
    numberOfChannels: stream.channels,
    description: flacHeader(stream),
  };
}

/** The stream the question decodes a frame of: wlshare's. */
const PROBE_STREAM: FlacStream = {
  sampleRate: 48_000,
  channels: 2,
  packetFrames: 960,
};

/**
 * A frame of music as the gateway sends one, base64: 20 ms of the FLAC
 * benchmark's `music` sample at 48 kHz in stereo, coded by `sound-flac` as wlshare
 * and the gateway code theirs. The one Safari's decoder failed.
 */
const PROBE_FRAME =
  "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";

/**
 * How long the decode may take to answer before it counts as a no: the page
 * mounts only once every answer is in.
 */
let attemptTimeoutMs = 2000;

let answer: boolean | null = null;

/** Whether a decoder configured for the probe's stream turns its frame into sound. */
async function decodesFrame(): Promise<boolean> {
  if (typeof AudioDecoder === "undefined") {
    return false;
  }
  const config = flacConfig(PROBE_STREAM);
  try {
    const support = await AudioDecoder.isConfigSupported(config);
    if (support.supported !== true) {
      return false;
    }
  } catch {
    return false;
  }
  let samples = 0;
  let failed = false;
  let decoder: AudioDecoder | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    decoder = new AudioDecoder({
      output: (data) => {
        samples += data.numberOfFrames;
        data.close();
      },
      error: () => {
        failed = true;
      },
    });
    decoder.configure(config);
    const data = Uint8Array.from(atob(PROBE_FRAME), (c) => c.charCodeAt(0));
    decoder.decode(new EncodedAudioChunk({ type: "key", timestamp: 0, data }));
    const unanswered = new Promise<never>((_, reject) => {
      timer = setTimeout(
        () => reject(new Error("no answer")),
        attemptTimeoutMs,
      );
    });
    await Promise.race([decoder.flush(), unanswered]);
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
    if (decoder && decoder.state !== "closed") {
      decoder.close();
    }
  }
  return !failed && samples === PROBE_STREAM.packetFrames;
}

/** Ask the browser once, and remember the answer. */
export async function askFlac(): Promise<void> {
  answer ??= await decodesFrame();
}

/**
 * Whether this browser's own decoder plays lossless sound. Only valid after
 * `askFlac` has resolved, which `main.tsx` awaits before mounting.
 */
export function decodesFlac(): boolean {
  if (answer === null) {
    throw new Error("decodesFlac() before askFlac() resolved");
  }
  return answer;
}

/** Test seam: forget the answer so the question can be asked again. */
export function resetFlacForTests(timeoutMs = 2000): void {
  answer = null;
  attemptTimeoutMs = timeoutMs;
}
