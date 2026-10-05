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

import type { HoldCause } from "./protocol.ts";

/**
 * The wire fields of `audioFormat`, minus the `OpusHead` bytes and the samples in
 * a packet.
 *
 * Only the decoder wants those; this describes the stream to a person, and the
 * client keeps exactly what it can show rather than parking a `Uint8Array` in React
 * state for the life of the session.
 */
export interface AudioStreamInfo {
  codec: string;
  sampleRate: number;
  channels: number;
  // The remote's own packets, passed through untouched — a High Performance
  // Mac's AAC-ELD, wlshare's Opus or FLAC — rather than coded by the gateway.
  passthrough: boolean;
}

/** Everything the Audio row is derived from. See `useRemoteDesktop`. */
export interface AudioRow {
  /** The session carries sound at all (`audio` on `connected`). */
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
 * The stream itself: codec, rate, channels, and whose stream it is, as the Video
 * row says of the picture.
 */
function streamLabel(stream: AudioStreamInfo): string {
  const shape = `${rateLabel(stream.sampleRate)} ${channelsLabel(stream.channels)}`;
  const whose = stream.passthrough
    ? "passthrough from the remote"
    : "encoded by the gateway";
  return `${stream.codec} · ${shape} · ${whose}`;
}

/**
 * The Audio row.
 *
 * The failure is reported ahead of everything else because it is the only state
 * here that is *wrong* rather than merely off.
 */
export function audioLabel(row: AudioRow): string {
  if (!row.available) {
    return "None in this session";
  }
  if (row.error) {
    return `Stopped — ${row.error}`;
  }
  if (!row.enabled) {
    return "Muted";
  }
  // Enabled is a click; the format is a round trip later, and the gap is real on a
  // remote that has to arm its audio bridge first.
  return row.stream ? streamLabel(row.stream) : "Waiting for the audio format";
}

/** The Render row: the dial this session resolved to. */
export function renderLabel(plan: string): string {
  return plan || "Waiting for the target";
}

/** The wire fields of `videoFormat`, or a pipeline this browser composes. */
export interface VideoStreamInfo {
  decode: string;
  // The remote's own stream, passed through untouched — wlshare's VP9, a High
  // Performance Mac's HEVC — rather than one the gateway encoded.
  passthrough: boolean;
  // Not a stream at all: an RDP host's graphics pipeline, composed here
  // (`graphicsStart`). No decoder is configured and `decode` names nothing.
  composed?: boolean;
}

/**
 * The Video row: the exact configuration the decoder was built with and whose
 * stream it decodes, or what the row is waiting for before the stream's format has
 * arrived. While the desktop is held — past what a video stream encodes, or All
 * Displays over too many screens — there is no picture, whatever decoder was
 * built before.
 */
export function videoLabel(
  stream: VideoStreamInfo | null,
  held: HoldCause | null,
): string {
  if (held === "size") {
    return "Not in use: the desktop is past what video carries";
  }
  if (held === "screens") {
    return "Not in use: Combined Display spans more than two screens";
  }
  if (!stream) {
    return "Waiting for the video format";
  }
  if (stream.composed) {
    return "Not in use: the host's graphics pipeline is composed by this browser";
  }
  return `${stream.decode} · ${stream.passthrough ? "passthrough from the remote" : "encoded by the gateway"}`;
}
