// What the "This session" card says about the sound and the video, in the same
// register as the Render row above them.
//
// A module of its own for the reason `connectionLabel.ts` is one: these are pure
// functions of what arrived on the wire, and a test of a string should not have to
// stand up a fake browser to import the component that shows it.
//
// The Render row already says what the *gateway* resolved to. These two say what
// this browser ended up doing with it, which is a different fact and the one that
// is otherwise invisible: `render` names a motion codec but never says a stream
// decoder was configured, and it says nothing at all about audio, whose format is
// announced only on the audio socket. Until they existed, "why is there no sound"
// and "which decoder is this browser running" were answerable only by reading the
// console.

/**
 * The wire fields of `audioFormat`, minus the `OpusHead` bytes.
 *
 * Only the decoder wants those; this describes the stream to a person, and the
 * client keeps exactly what it can show rather than parking a `Uint8Array` in React
 * state for the life of the session.
 */
export interface AudioStreamInfo {
  codec: string;
  sampleRate: number;
  channels: number;
  packetFrames: number;
}

/** Everything the Audio row is derived from. See `useRemoteDesktop`. */
export interface AudioRow {
  /** The target offered sound at all (`audio` on `connected`). */
  available: boolean;
  /** This browser asked for it. Never proof that any is arriving. */
  enabled: boolean;
  /** A decoder that refused or failed, which is also why `enabled` went false. */
  error: string | null;
  /** The format the decoder was built from, or null before one arrived. */
  stream: AudioStreamInfo | null;
}

// 48 kHz, written the way somebody comparing it to a device's rate would say it.
// A fractional rate such as 44.1 kHz keeps its fraction.
function rateLabel(hz: number): string {
  const khz = hz / 1000;
  return `${Number.isInteger(khz) ? khz : khz.toFixed(1)} kHz`;
}

function channelsLabel(count: number): string {
  if (count === 1) {
    return "mono";
  }
  return count === 2 ? "stereo" : `${count} channels`;
}

/**
 * The stream itself: codec, rate, channels, and how much sound is in one packet.
 *
 * The packet length is given in milliseconds rather than as `packetFrames`,
 * because that is the figure a person can compare to what they are hearing.
 */
function streamLabel(stream: AudioStreamInfo): string {
  const shape = `${rateLabel(stream.sampleRate)} ${channelsLabel(stream.channels)}`;
  const ms = (stream.packetFrames / stream.sampleRate) * 1000;
  return `${stream.codec} · ${shape} · ${Number(ms.toFixed(1))} ms packets`;
}

/**
 * The Audio row.
 *
 * The failure is reported ahead of everything else because it is the only state
 * here that is *wrong* rather than merely off.
 */
export function audioLabel(row: AudioRow): string {
  if (!row.available) {
    return "Not offered by this target";
  }
  if (row.error) {
    return `Stopped — ${row.error}`;
  }
  if (!row.enabled) {
    return "Available, not playing";
  }
  // Enabled is a click; the format is a round trip later, and the gap is real on a
  // remote that has to arm its audio bridge first.
  return row.stream ? streamLabel(row.stream) : "Waiting for the audio format";
}

/**
 * The Render row: the dial this session resolved to, or — while the desktop is past
 * what a video stream encodes — the tiles that carry it instead.
 */
export function renderLabel(plan: string, tiling: boolean): string {
  if (!plan) {
    return "Waiting for the target";
  }
  return tiling ? "PNG tiles: the desktop is past what video carries" : plan;
}

/**
 * The Video row: the exact configuration the decoder was built with, or what the
 * row is waiting for before the stream's format has arrived. While the picture is
 * tiles no decoder is in use, whatever one was built before.
 */
export function videoLabel(decode: string | null, tiling: boolean): string {
  if (tiling) {
    return "Not in use: the picture is PNG tiles";
  }
  return decode ?? "Waiting for the video format";
}
