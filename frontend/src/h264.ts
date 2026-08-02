// Reading an H.264 Annex-B access unit well enough to hand it to WebCodecs.
//
// A target on `render_type = "video"` sends the whole desktop as one inter-frame
// stream instead of one encoded image per changed region, and each TILE record
// carries exactly one access unit (see `Tile::FORMAT_H264` in src/protocol.rs). Two
// things have to be read out of those bytes before a `VideoDecoder` can take them:
//
//   - whether this unit is a keyframe, because an `EncodedVideoChunk` is typed
//     "key" or "delta" and a decoder cannot start on a delta;
//   - the SPS's profile, constraint flags and level, because `configure` wants a
//     codec string of the form `avc1.PPCCLL` and nothing else on the wire says.
//
// Deliberately not a full parser. Nothing here decodes a slice or reads an SPS past
// its first three bytes — those three are all the codec string needs, and inventing
// more parser than that would be inventing more to be wrong about.
//
// Modelled on noVNC's decoder, which is the reference implementation of this exact
// parse.

/** NAL unit types this cares about. Everything else is passed along unread. */
const NAL_NON_IDR_SLICE = 1;
const NAL_IDR_SLICE = 5;
const NAL_SPS = 7;

/** What a decoder needs to be told before it can be handed a chunk. */
export interface AccessUnit {
  /** True when a decoder with no history can start here. */
  key: boolean;
  /**
   * The `avc1.PPCCLL` codec string from this unit's SPS, or null when it carries
   * none. Every keyframe the gateway sends carries one, because its encoder repeats
   * SPS and PPS with every keyframe precisely so a client needs nothing out of band.
   */
  codec: string | null;
}

/**
 * The length of the Annex-B start code at `at`, or 0 if there is not one there.
 *
 * Both lengths are legal and both occur in one stream: openh264 writes the four-byte
 * form ahead of parameter sets and the three-byte form elsewhere.
 */
function startCodeLength(data: Uint8Array, at: number): number {
  if (
    data[at] === 0 &&
    data[at + 1] === 0 &&
    data[at + 2] === 0 &&
    data[at + 3] === 1
  ) {
    return 4;
  }
  if (data[at] === 0 && data[at + 1] === 0 && data[at + 2] === 1) {
    return 3;
  }
  return 0;
}

/** Where the next start code begins at or after `from`, or the end of the data. */
function nextStartCode(data: Uint8Array, from: number): number {
  for (let at = from; at < data.length; at += 1) {
    if (startCodeLength(data, at) !== 0) {
      return at;
    }
  }
  return data.length;
}

/**
 * Read one access unit's NAL units, reporting what a decoder has to be told.
 *
 * Returns null for anything that is not an Annex-B stream at all — a payload that
 * does not begin with a start code, or one with no NAL unit in it. A caller drops
 * such a record rather than handing it to a decoder, the same way `decodeBatchFrame`
 * drops a malformed frame: guessing at a video stream's bytes goes wrong quietly and
 * stays wrong until the next keyframe.
 */
export function readAccessUnit(data: Uint8Array): AccessUnit | null {
  let key = false;
  let codec: string | null = null;
  let sliced = false;

  let at = 0;
  while (at < data.length) {
    const nal = readNal(data, at);
    if (!nal) {
      return null;
    }
    if (nal.type === NAL_IDR_SLICE) {
      key = true;
      sliced = true;
    } else if (nal.type === NAL_NON_IDR_SLICE) {
      sliced = true;
    } else if (nal.type === NAL_SPS) {
      codec = codecString(data, nal.start) ?? codec;
    }
    at = nextStartCode(data, nal.start + 1);
  }

  // A unit with parameter sets and no slice paints nothing and would leave a pending
  // decode with no frame ever coming, which is worse than dropping it.
  return sliced ? { key, codec } : null;
}

/**
 * The NAL unit beginning at `at`, or null when there is not one there.
 *
 * Null covers all three ways these bytes can fail to be the stream they claim to be:
 * no start code where one must begin (including at the very front, which is how a
 * payload that is not Annex-B at all is caught), a start code with nothing after it,
 * and a header whose forbidden_zero_bit is set.
 */
function readNal(
  data: Uint8Array,
  at: number,
): { type: number; start: number } | null {
  const header = startCodeLength(data, at);
  if (header === 0) {
    return null;
  }
  const start = at + header;
  if (start >= data.length || (data[start] & 0x80) !== 0) {
    return null;
  }
  return { type: data[start] & 0x1f, start };
}

/**
 * `avc1.PPCCLL` from the SPS whose header is at `start`: profile_idc, the
 * constraint_set flags byte, and level_idc, which are the three bytes immediately
 * after the header and the whole of what a codec string is made of.
 */
function codecString(data: Uint8Array, start: number): string | null {
  if (start + 3 >= data.length) {
    return null;
  }
  return `avc1.${hex(data[start + 1])}${hex(data[start + 2])}${hex(data[start + 3])}`;
}

function hex(byte: number): string {
  return byte.toString(16).padStart(2, "0");
}
