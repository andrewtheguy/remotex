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
// decoder was configured, and it says nothing at all about audio, whose codec is a
// per-target choice made in the operator's config file (`audio_codec`) and
// announced only on the audio socket. Until they existed, "why is there no sound"
// and "which decoder is this browser running" were answerable only by reading that
// file and the console.

import { PCM_CODEC } from "./audioPlayer.ts";

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

// 48 kHz, 44.1 kHz — the two this path actually produces, written the way somebody
// comparing them to a device's rate would say them.
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
 * because that is the figure a person can compare to what they are hearing — and
 * passthrough has none to give, its packets being whatever length the remote's
 * wave buffers were. Naming it as passthrough is the point of that branch: no
 * decoder ran, so a browser's codec support cannot be what is wrong.
 */
function streamLabel(stream: AudioStreamInfo): string {
  const shape = `${rateLabel(stream.sampleRate)} ${channelsLabel(stream.channels)}`;
  if (stream.codec === PCM_CODEC) {
    return `${stream.codec} · ${shape} · passthrough, no decoder`;
  }
  const ms = (stream.packetFrames / stream.sampleRate) * 1000;
  return `${stream.codec} · ${shape} · ${Number(ms.toFixed(1))} ms packets`;
}

/**
 * The Audio row.
 *
 * The failure is reported ahead of everything else because it is the only state
 * here that is *wrong* rather than merely off, and because it has nowhere else to
 * appear in `remotex.app`: the drawer that shows it in a browser is exactly the
 * chrome that host replaces (see `chromeless` in FloatingMenu).
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
 * The Video row: the exact configuration every decoder this attachment configured
 * was built with, or null for a session that has no video in it at all.
 *
 * Null rather than "none", and that is why this returns one: under every tile-only
 * dial there is no decoder to describe and the Render row above has already said
 * so, so the honest thing is not to print a row. A motion-stream session that has
 * simply not moved yet is the same case — the row appears when the first stream
 * does.
 *
 * The count is stream ids, which is what the client holds rather than what is
 * moving this instant: ids are reused as regions come and go and nothing on the
 * wire retires one, so a decoder configured for a region that has since gone quiet
 * is still a decoder. Distinct strings rather than one line per stream, because the
 * configuration carries a size-derived level — four streams over four region sizes
 * may name four different ones or all the same, and this says which in one line.
 */
export function videoLabel(decodes: readonly string[]): string | null {
  if (decodes.length === 0) {
    return null;
  }
  const distinct = [...new Set(decodes)].sort();
  if (decodes.length === 1) {
    return distinct[0];
  }
  return `${decodes.length} streams · ${distinct.join(", ")}`;
}
