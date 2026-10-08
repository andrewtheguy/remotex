// What this browser decodes of a High Performance Mac's own media stream: the second
// question about its decoders the gateway is told, asked once, before the client
// mounts, beside the chroma (videoChroma.ts).
//
// The two halves are separate questions with separate consequences:
// - The picture. A session started with the Mac's picture passed sends its HEVC as
//   it came, instead of VP9 encoded from decoded pictures. The picker offers that
//   choice to a browser that says yes here, or whose page can decode the picture
//   itself, and greys it for any other, which starts the target with VP9. The answer rides every
//   session socket this page opens (`gateway.ts`), for the same reason the chroma
//   does: a page that comes back to its session saying no is returned to the
//   picker instead of being sent what it cannot decode.
// - The sound. Every session on such a Mac is sent its AAC-ELD as it came: the
//   gateway has no decoder for it. So this answer chooses nothing and is told to
//   nobody. It is which form of the configuration the player uses
//   (`appleEldConfig`), and a browser with none plays the session without sound
//   and says why.
//
// They are asked differently:
// - The picture, HEVC Range Extensions 4:4:4: `VideoDecoder.isConfigSupported`, which
//   answered as the decoder then behaved on every browser measured.
// - The sound: `AudioDecoder.isConfigSupported`, then a real decode of one of the
//   Mac's own units in each form it says yes to. No codec string names AAC-ELD to both
//   Chrome and Safari — both refuse `mp4a.40.39` — so both are asked for AAC's
//   `mp4a.40.2`, and they need the AudioSpecificConfig differently: Chrome as it is,
//   Safari inside an MPEG-4 ES_Descriptor, since the CoreAudio call WebKit reads it
//   with refuses a bare one and WebKit then decodes as AAC-LC without it. Both say
//   yes to both forms, so a yes only narrows the forms worth decoding and decoding
//   picks one.
//
// The picture's answer is the browser's own decoder's and nothing else's. A
// picture it refuses can still be decoded by the page, in a session started so
// at the picker (targetChoices.ts): that is the session's choice, which the
// gateway holds and tells every page of, and not this page's answer.
//
// The other way round from the chroma on a doubt. VP9 is what every browser here
// decodes, so only a definite "yes" offers the Mac's picture, and anything that
// throws reads as "no". The one target it can leave unstartable is a Mac on a gateway
// whose host lacks the HEVC decoder's library, which has no other picture to send.

/**
 * The picture asked about: macwork's stream, 1600×1000, as its sequence parameter
 * set names it (`parse_sps` in src/vnc_apple_media.rs) — Range Extensions, level 5.0,
 * the 4:4:4 constraint flags.
 */
const APPLE_HEVC_PROBE = "hev1.4.10.L150.BE.8";

/** The codec string the gateway names the Mac's passed sound with (`PASSED_SOUND`). */
export const APPLE_ELD_CODEC = "mp4a.40.39";

/** What both browsers accept AAC-ELD under: AAC's own string, not ELD's. */
const ELD_DECODE_CODEC = "mp4a.40.2";

/** The Mac's AudioSpecificConfig (src/aac_eld.rs): ELD, 48 kHz, stereo, 480 frames. */
const ELD_CONFIG = Uint8Array.of(0xf8, 0xe6, 0x50, 0x00);

/** One of the Mac's 10 ms units, captured with `tests/hp_capture.sh`, to decode. */
const ELD_UNIT =
  "if////wdb7QoEQX5+fPnj64ZercOrmZxlTXLPDnPQjO35Hiu7IH2xLp/sEn0L1kRq36BpEhJqdBEZM6DJHKTXVI365CnaJYPFk4l8jSwxA6CS5OQ1EZkuzIRKTMJvjkbMshPhkiyicnFkaWdoRpJ9cmq2RhYuxDEgUSYbxHJ5m7K1pYbBcQR0eWrDBEhVMh4cjpcSRrkJDiSbbI3rBFo/21Q1yNuZYoO0smxyMtsuhr4ie+RwsuzT7UIpvEb0miQPBEc0jOZ8y4SIoxGMfkmGkRQyMYnYMgkSRyMg/dOIkRxiMY36XukiuuRqv/O/MkV1SNRujcukQOItBiuac6tItBpHNOdW49ZiOK51Zj1eI4rnVePV4jiuTVYCrEcJuklikp2K3SOxR07FbpHYo6dhtuisUdOw3feFak77wrUnfeFak77wrUnMeFak5jwrUnMeFak5jtrUm5xWTXOKyaxxWTWOKyaTNZNJmsmkzWTSZrJs8OeHPDnhkQ95AA=";

/**
 * How long one decode attempt may
 * take to answer before it counts as a no: the page mounts only once every answer is
 * in.
 */
let attemptTimeoutMs = 2000;

/**
 * An AudioSpecificConfig inside an MPEG-4 ES_Descriptor (ISO/IEC 14496-1): ES_ID 1,
 * and a DecoderConfigDescriptor for MPEG-4 audio (0x40, an audio stream) whose
 * DecoderSpecificInfo is `config`. What CoreAudio's `kAudioFormatProperty_FormatInfo`
 * reads an AAC configuration from.
 */
export function esDescriptor(config: Uint8Array): Uint8Array {
  const specific = [0x05, config.length, ...config];
  const decoderConfig = [
    0x04,
    13 + specific.length,
    0x40,
    0x15,
    ...[0x00, 0x18, 0x00],
    ...[0, 0, 0, 0],
    ...[0, 0, 0, 0],
    ...specific,
  ];
  return Uint8Array.from([
    0x03,
    3 + decoderConfig.length,
    0x00,
    0x01,
    0x00,
    ...decoderConfig,
  ]);
}

/** How a browser's `AudioDecoder` takes the Mac's configuration as `description`. */
type Description = (config: Uint8Array) => Uint8Array;

const FORMS: Description[] = [(config) => config, esDescriptor];

let answer: boolean | null = null;

/**
 * The form the sound decoded in, null for neither, and undefined until the
 * question, which `chooseAppleMedia` starts and does not wait for, is answered.
 */
let soundForm: Description | null | undefined;
let soundProbe: Promise<void> | null = null;

async function decodesPicture(): Promise<boolean> {
  try {
    const support = await VideoDecoder.isConfigSupported({
      codec: APPLE_HEVC_PROBE,
    });
    return support.supported === true;
  } catch {
    return false;
  }
}

/**
 * Whether the browser says it takes `description`, and a decoder configured with it
 * then turns the unit into sound.
 */
async function decodesSound(description: Uint8Array): Promise<boolean> {
  const config: AudioDecoderConfig = {
    codec: ELD_DECODE_CODEC,
    sampleRate: 48_000,
    numberOfChannels: 2,
    description,
  };
  try {
    const support = await AudioDecoder.isConfigSupported(config);
    if (support.supported !== true) {
      return false;
    }
  } catch {
    return false;
  }
  let output = false;
  let failed = false;
  let decoder: AudioDecoder | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    decoder = new AudioDecoder({
      output: (data) => {
        output = true;
        data.close();
      },
      error: () => {
        failed = true;
      },
    });
    decoder.configure(config);
    const data = Uint8Array.from(atob(ELD_UNIT), (c) => c.charCodeAt(0));
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
  return output && !failed;
}

async function probeSound(): Promise<void> {
  let found: Description | null = null;
  for (const form of FORMS) {
    if (await decodesSound(form(ELD_CONFIG))) {
      found = form;
      break;
    }
  }
  soundForm = found;
}

/**
 * Ask the browser once, and remember the answers. Resolves to whether it decodes
 * the Mac's picture, which is what `decodesAppleMedia()` then says. The sound's
 * question is started here and not waited for: a decoder that never answers
 * takes each form's whole timeout, and only a Mac's sound needs the answer
 * (`appleSoundProbed`), so the page does not mount behind it.
 */
export async function chooseAppleMedia(): Promise<boolean> {
  if (answer !== null) {
    return answer;
  }
  soundProbe ??= probeSound();
  answer = await decodesPicture();
  return answer;
}

/**
 * Resolves once the sound's question is answered, which `appleEldConfig` needs.
 * Null where it already is, so a caller can carry on in the same turn.
 */
export function appleSoundProbed(): Promise<void> | null {
  if (soundForm !== undefined) {
    return null;
  }
  soundProbe ??= probeSound();
  return soundProbe;
}

/**
 * Whether this browser's own decoder takes the Mac's picture. Only valid after
 * `chooseAppleMedia` has resolved, which `main.tsx` awaits before mounting.
 */
export function decodesAppleMedia(): boolean {
  if (answer === null) {
    throw new Error("decodesAppleMedia() before chooseAppleMedia() resolved");
  }
  return answer;
}

/**
 * The decoder configuration for the Mac's sound, as announced (`sampleRate`,
 * `channels`, and its AudioSpecificConfig as `head`), in the form this browser
 * decoded at load. Throws, in words for the session's Audio row, in a browser
 * that decoded neither form: the session then plays without sound.
 */
export function appleEldConfig(format: {
  sampleRate: number;
  channels: number;
  head: Uint8Array;
}): AudioDecoderConfig {
  if (soundForm === undefined) {
    throw new Error("appleEldConfig() before appleSoundProbed() resolved");
  }
  const form = soundForm;
  if (!form) {
    throw new Error("This browser does not decode the Mac's AAC-ELD sound.");
  }
  return {
    codec: ELD_DECODE_CODEC,
    sampleRate: format.sampleRate,
    numberOfChannels: format.channels,
    description: form(format.head),
  };
}

/** Test seam: forget the answer so the question can be asked again. */
export function resetAppleMediaForTests(timeoutMs = 2000): void {
  answer = null;
  soundForm = undefined;
  soundProbe = null;
  attemptTimeoutMs = timeoutMs;
}
