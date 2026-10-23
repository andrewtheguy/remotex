// What a session is started with: how the desktop is sized, whether the remote's
// sound is taken and as what, which of the target's own streams is passed if any,
// and where a second virtual display sits. Chosen under the target at the picker, before
// Start, and held for the life of the session — `connect` carries them and the
// gateway keeps them beside the target (src/config.rs, `Choices`).
//
// A module of its own because the rules are a pure function of three things — what
// the target's type offers (GET /api/targets), what this browser can take, and what
// was chosen last time — and the component that shows them cannot be imported
// without a browser.
//
// The rules:
// - The size a session will have is always shown, and is a choice only where
//   there are two: see `sizeOptions`.
// - An option the target's type does not offer has no row.
// - One it offers that cannot be had here is greyed, with the reason: Opus in
//   a browser with no audio decoder, or a passthrough this browser cannot take,
//   a picture its decoder refuses among them.
// - An RDP host's graphics pipeline is passed in every session, which is said and
//   not offered: the gateway composes none.
// - A High Performance Mac's picture is one of two, both passed through: its
//   HEVC, or its H.264, in that order: see `pictureOptions`. The first this
//   browser can take is taken until another is chosen, and where it takes
//   neither the target cannot start.
// - A Standard Mac's screens are shown one at a time or all at once, as Combined
//   Display: see `VIEWS`. Screen Sharing Standard's alone; no other target has a
//   combined view. One at a time until Combined is chosen.
// - A Standard Mac's picture shown one screen at a time is one of two: the Mac's
//   own Adaptive picture passed through (beta), which this page decodes, or the
//   gateway's tiles of the Mac's Full quality, which settle to lossless, named by
//   no passthrough: see `pictureOptions`. The Adaptive one until the other is
//   chosen, and neither greyed. Combined Display is the tiles of every screen, so
//   the Adaptive picture is greyed under it.
// - A `wlshare` target's picture is one of two, both wlshare's own, passed
//   through: its VP9 of the whole desktop, which fits the link, or its tiles with
//   motion, exact PNG where the desktop is still and video only where it moves, for
//   a LAN: see `pictureOptions`. The VP9 until the other is chosen, and neither
//   greyed: both need the VP9 decoder the target cannot start without.
// - Sound is ticked or not where the target offers it, and a ticked one is Opus
//   or lossless: see `soundRow`. Ticking takes Opus. A browser with no audio
//   decoder has no sound to tick, and one whose decoder does not play FLAC is
//   not offered lossless.
// - Where the second display sits is a choice on a target that asks its host for
//   two and tells it where each is: see `PLACEMENTS`.
// - This browser's camera and microphone are offered to the remote or not, each
//   ticked where the target has somewhere to put it, an RDP host or wlshare:
//   see `deviceRow`. Both experimental, and neither until somebody ticks it. The
//   tick is the offer alone: the device is turned on from the session's menu.
// - How the host draws an RDP pipeline, high quality or adaptive H.264 (alpha), is a
//   choice on a target with the pipeline on, and under adaptive the whole desktop
//   or by region: see `pipelineRow`. A browser that cannot decode the H.264 is
//   offered high quality alone.
// - A target that can only be passed, in a browser that cannot take it, cannot
//   start, and Start says so before the remote is dialled: an RDP host's
//   pipeline in a page that cannot compose it, and a `wlshare` target's pictures,
//   VP9 both, in a browser whose decoder refuses VP9.
// - Which decoder takes the picture is no choice: the browser's own decodes the
//   VP9, and the page a Mac's passed HEVC where the gateway serves the decoder
//   for it, in WebAssembly wherever the browser's own decoder does not take it
//   (softwareDecoder.ts).

/** A stream a target can pass untouched, as `/api/targets` names it. */
export type Passthrough =
  | "rdp-graphics"
  | "apple-media"
  | "apple-h264"
  | "apple-adaptive"
  | "wlshare-vp9"
  | "wlshare-tiles-motion";

/** One entry of GET /api/targets. */
export interface TargetInfo {
  name: string;
  protocol: string;
  // The target's `subtype` where it has one, null otherwise. Shown because four
  // entries in this list can say `vnc` and mean a plain server, a wlshare one, a
  // Mac sharing its physical displays, and a Mac on one virtual display it will
  // disable them for — which is a difference somebody is choosing between here,
  // not discovering after connecting. See connectionLabel.ts.
  subtype: string | null;
  host: string;
  port: number;
  /** Whether the window can drive the desktop's size. */
  resize: boolean;
  /** The size the operator configured for the target, null where there is none. */
  size: Points | null;
  /**
   * The size a session keeps where none is configured. Null on a target no
   * session states a size for: a Mac sharing its physical displays.
   */
  defaultSize: Points | null;
  /** Whether the remote's sound is a choice. */
  audio: boolean;
  /**
   * Whether a page whose own decoder refuses this target's passed HEVC can
   * decode it in the software decoder the gateway serves.
   */
  software: boolean;
  /**
   * The streams this target can pass, in the picker's order: none, an RDP host's
   * graphics pipeline, a High Performance Mac's HEVC and its H.264, a Standard
   * Mac's Adaptive picture, or wlshare's VP9 and its tiles with motion.
   */
  passthroughs: Passthrough[];
  /** Whether where the second virtual display sits is a choice. */
  placement: boolean;
  /** Whether how the host draws the graphics pipeline is a choice. */
  pipeline: boolean;
  /**
   * Whether a Standard Mac's screens are shown one at a time or as Combined
   * Display: Screen Sharing Standard's alone.
   */
  standardView: boolean;
  /** Whether offering the remote this browser's camera is a choice (experimental). */
  camera: boolean;
  /** Whether offering the remote this browser's microphone is a choice (experimental). */
  microphone: boolean;
}

/** A desktop size in points. */
export interface Points {
  w: number;
  h: number;
}

/**
 * How a session's desktop is sized, as `connect` names it: kept at the target's
 * size (the configured one, or the default where it has none), kept at the
 * default on a target that configures another, or driven by this client's window.
 */
export type Sizing = "target" | "default" | "window";

/**
 * Whether a session takes the remote's sound, as `connect` names it, and what
 * the sound is sent as: Opus, or FLAC, which is lossless.
 */
export type Sound = "off" | "opus" | "flac";

/**
 * Where the second of two virtual displays sits against the first, as `connect`
 * names it.
 */
export type Placement = "right" | "left" | "top" | "bottom";

/**
 * How the host draws an RDP session's graphics pipeline, as `connect` names it:
 * in high quality, without H.264, or adaptive, in H.264 the host fits to the link, over
 * the whole desktop or only where it moves like video.
 */
export type Pipeline = "high-quality" | "adaptive-desktop" | "adaptive-region";

/**
 * How a Standard Mac's screens are shown, as `connect` names it: one at a time, its
 * own picture passed, or all at once as Combined Display, which the gateway codes.
 */
export type StandardView = "one" | "combined";

/** What `connect` carries. */
export interface Choices {
  size: Sizing;
  audio: Sound;
  /**
   * The stream of the target's to pass, where which is a choice: a Mac's HEVC,
   * its H.264 or its Adaptive picture, or wlshare's VP9 or its tiles with motion.
   * Null where none is passed: a target whose picture the gateway codes, a
   * Standard Mac's whose tiles were chosen over its Adaptive picture, or one that
   * passes a stream in every session, which is not named.
   */
  passthrough: Passthrough | null;
  placement: Placement;
  pipeline: Pipeline;
  standardView: StandardView;
  /** Whether the remote is offered this browser's camera (experimental). */
  camera: boolean;
  /** Whether the remote is offered this browser's microphone (experimental). */
  microphone: boolean;
}

/** What this browser can do with a target. */
export interface Abilities {
  /**
   * What it said it can take, as its session socket states it. `appleMedia` is
   * its own decoder's answer about a Mac's HEVC.
   */
  appleMedia: boolean;
  rdpGraphics: boolean;
  /** Whether it decodes the H.264 an RDP host may draw with (rdpH264.ts). */
  rdpH264: boolean;
  /**
   * Whether it has a `VideoDecoder`, taken to decode a High Performance Mac's
   * H.264 without asking, and whether that decoder takes wlshare's VP9 at 4:2:0
   * (videoCodecs.ts).
   */
  appleH264: boolean;
  vp9: boolean;
  /** Whether this browser has an audio decoder, which every sound needs. */
  audio: boolean;
  /**
   * Whether that decoder plays lossless sound, FLAC, which Safari's does not
   * (flacSound.ts).
   */
  lossless: boolean;
  /** Whether this page can run its software HEVC decoder (softwareSupport.ts). */
  runsHevc: boolean;
  /**
   * What of this client a desktop can follow: its window on a desktop browser, its
   * screen, asked for once, on a tablet, and nothing on a phone, whose window is
   * no desktop's shape.
   */
  follows: "window" | "screen" | null;
}

/** One way a target's desktop can be sized. */
export interface SizeOption {
  value: Sizing;
  /** The size, as Start will ask for it. */
  label: string;
  note: string;
}

/** One format a target's sound can be sent as. */
export interface SoundFormat {
  value: Exclude<Sound, "off">;
  label: string;
  /** What choosing it does, or why it cannot be chosen here. */
  note: string;
  disabled: boolean;
}

/** Whether a target's sound is taken, and the formats a ticked one chooses between. */
export interface SoundRow {
  label: string;
  note: string;
  checked: boolean;
  /** Whether it cannot be ticked: no format plays here. */
  disabled: boolean;
  /** Opus first, which ticking it takes, then lossless. */
  formats: SoundFormat[];
}

/** A device of this browser's the remote can be offered: the camera or the microphone. */
export interface DeviceRow {
  label: string;
  note: string;
  checked: boolean;
}

/** One way the host can draw the pipeline, or one way of drawing it adaptive. */
export interface PipelineOption<V> {
  value: V;
  label: string;
  /** What choosing it does, or why it cannot be chosen here. */
  note: string;
  disabled: boolean;
}

/**
 * How the host draws the pipeline: high quality or adaptive, and under an adaptive
 * one, the whole desktop or by region.
 */
export interface PipelineRow {
  modes: PipelineOption<"high-quality" | "adaptive">[];
  h264: PipelineOption<Exclude<Pipeline, "high-quality">>[];
}

/** One way a Standard Mac's screens can be shown. */
export interface ViewOption {
  value: StandardView;
  label: string;
  note: string;
}

/** One place the second display can sit. */
export interface PlacementOption {
  value: Placement;
  label: string;
}

/** A stream that is passed in every session, said rather than offered. */
export interface PassedNote {
  label: string;
  /** Why it is always passed. */
  note: string;
}

/** One way a Mac's or a wlshare desktop's picture can reach this browser. */
export interface PictureOption {
  /** What `connect` names: the stream passed, or null for the gateway's tiles. */
  value: Exclude<Passthrough, "rdp-graphics"> | null;
  label: string;
  /** What choosing it does, or why it cannot be chosen here. */
  note: string;
  disabled: boolean;
}

/** A target's options as the picker shows them. */
export interface TargetOptions {
  /**
   * The ways this target's desktop can be sized here: never empty, a choice
   * where there are two, and otherwise the one size the session will have.
   */
  sizes: SizeOption[];
  /** The target's sound, as a choice: null where it offers none. */
  soundRow: SoundRow | null;
  /** This browser's camera, offered to the remote: null where the target has nowhere to put it. */
  cameraRow: DeviceRow | null;
  /** This browser's microphone, offered to the remote: null where the target has nowhere to put it. */
  microphoneRow: DeviceRow | null;
  /**
   * Where the second display can sit, on a target whose host is told: null
   * where it is not a choice.
   */
  placements: PlacementOption[] | null;
  /** How the host draws the pipeline, as a choice: null where it is none. */
  pipelineRow: PipelineRow | null;
  /** How a Standard Mac's screens are shown, as a choice: null elsewhere. */
  views: ViewOption[] | null;
  /** The target's stream where it is passed in every session: null otherwise. */
  passed: PassedNote | null;
  /**
   * The ways the target's picture can reach this browser, where that is a
   * choice: null otherwise.
   */
  pictures: PictureOption[] | null;
  /** What Start sends. */
  choices: Choices;
  /** Whether a session started with them carries the remote's sound. */
  sound: boolean;
  /** Why the target cannot start in this browser, or null. */
  blocked: string | null;
}

// An RDP host's graphics pipeline: passed in every session of a target that has
// it on, so said and not offered.
const RDP_GRAPHICS: PassedNote & { blocked: string } = {
  label: "Always passed through",
  note: "egfx is on for this target, so every session passes the host's graphics pipeline through, for this browser to compose.",
  blocked:
    "This browser cannot compose the host's graphics pipeline, which this target always passes: that needs WebGL 2 and a cross-origin isolated page. Use a browser that can, or a target with egfx disabled.",
};

// Why a `wlshare` target cannot start in a browser that does not decode VP9,
// which both of its pictures are coded in.
const NO_VP9 =
  "This browser does not decode VP9, which both of this wlshare target's pictures are coded in. Use a browser that does, or a plain vnc target for the same server, which is sent tiles.";

// Why a High Performance Mac cannot start in a browser that takes neither picture.
const NO_APPLE_PICTURE =
  "This browser decodes neither of the Mac's pictures, its HEVC or its H.264, which are all this target sends. Use a browser that does, or the Mac's Screen Sharing Standard.";

/**
 * Why a Mac's HEVC is greyed here: the browser's own decoder refuses it, and the
 * page cannot stand in, for want of a decoder from this gateway or of what the
 * decoder runs on (softwareSupport.ts).
 */
function noHevc(target: TargetInfo): string {
  return target.software
    ? "This browser does not decode the Mac's HEVC, and this page cannot decode it itself: that needs WebCodecs, WebGL 2 with Display P3, WebAssembly SIMD and a cross-origin isolated page."
    : "This browser does not decode the Mac's HEVC, and this gateway serves no decoder for this page to decode it with.";
}

/**
 * The ways a Mac's or a wlshare desktop's picture can reach this browser, in the
 * order the picker shows them: a High Performance Mac's HEVC passed through, and
 * its H.264; a Standard Mac's Adaptive picture passed through, and the gateway's
 * tiles of its Full quality; wlshare's VP9 passed through, and its tiles with
 * motion.
 *
 * - The HEVC is greyed in a browser whose own decoder does not take it, unless
 *   the page decodes it itself, which it then does (softwareDecoder.ts). The
 *   socket states both answers, and the gateway passes the picture to a page that
 *   has either.
 * - The H.264 is greyed in a browser with no `VideoDecoder`: no page decoder
 *   stands in for it. Any that has one is taken to decode it, unasked.
 * - The Mac's Adaptive picture is never greyed, which every page decodes in WebAssembly
 *   (standardPicture.ts), nor the gateway's tiles, images every page decodes.
 * - Neither of wlshare's is greyed: both are VP9, which a `wlshare` target cannot
 *   start without (`blockedReason`), and the browser's own decoder takes it.
 */
function pictureOptions(
  target: TargetInfo,
  abilities: Abilities,
): PictureOption[] {
  const hevc = abilities.appleMedia || (target.software && abilities.runsHevc);
  const options: Record<Exclude<Passthrough, "rdp-graphics">, PictureOption> = {
    "apple-media": {
      value: "apple-media",
      label: "HEVC, passed through",
      note: hevc
        ? "The Mac's own picture in 4:4:4. Recommended, for the best quality."
        : noHevc(target),
      disabled: !hevc,
    },
    "apple-h264": {
      value: "apple-h264",
      label: "H.264 (alpha), passed through",
      note: abilities.appleH264
        ? "The Mac's own picture in 4:2:0, in the browser's own decoder. For compatibility."
        : "This browser has no video decoder for the Mac's H.264.",
      disabled: !abilities.appleH264,
    },
    "apple-adaptive": {
      value: "apple-adaptive",
      label: "Apple's Adaptive (beta), passed through",
      note: "The Mac's own Adaptive quality, as Apple's viewer has it: a coarse pass and its refinement, the same on any link, where a slower link gets fewer updates. Decoded by this page, and never lossless.",
      disabled: false,
    },
    "wlshare-vp9": {
      value: "wlshare-vp9",
      label: "Video, passed through",
      note: "wlshare's VP9 of the whole desktop, which it fits to this browser's link. For any link; never lossless.",
      disabled: false,
    },
    "wlshare-tiles-motion": {
      value: "wlshare-tiles-motion",
      label: "Tiles with motion, passed through",
      note: "wlshare's own tiles: exact PNG where the desktop is still, video only where it moves (H.264 by the host's GPU where it has one, VP9 otherwise), exact again once it stops. Several times the bytes. For a LAN.",
      disabled: false,
    },
  };
  const offered = target.passthroughs.flatMap((passthrough) =>
    passthrough === "rdp-graphics" ? [] : [options[passthrough]],
  );
  // A Standard Mac's other picture is coded here, and names no passthrough.
  return target.passthroughs.includes("apple-adaptive")
    ? [...offered, GATEWAY_ADAPTIVE]
    : offered;
}

// The gateway's own picture of a Standard Mac: its lossless Full quality, zlib,
// inflated by the gateway and coded as the tiles, as any picture coded here is.
const GATEWAY_ADAPTIVE: PictureOption = {
  value: null,
  label: "Gateway adaptive, settles to lossless",
  note: "The Mac's lossless Full quality, coded by the gateway as tiles that fit this browser's link: JPEG while a region changes, exact once it has settled. Resumes when this browser comes back.",
  disabled: false,
};

const FOLLOWS: Record<"window" | "screen", SizeOption> = {
  window: {
    value: "window",
    label: "This window's size",
    note: "The desktop follows the window as it changes.",
  },
  screen: {
    value: "window",
    label: "This screen's size",
    note: "Asked for once, in landscape. Rotating does not change it.",
  },
};

/**
 * The ways `target`'s desktop can be sized from a client that `follows`.
 *
 * - A target the window cannot drive keeps one size: the configured one, or the
 *   default.
 * - One it can drive follows a desktop browser's window or a tablet's screen, and
 *   offers the configured size beside that where there is one.
 * - A phone has nothing a desktop could follow, so there the choice is between the
 *   configured size and the default.
 */
function sizeOptions(
  target: TargetInfo,
  follows: Abilities["follows"],
): SizeOption[] {
  if (!target.defaultSize) {
    return [
      {
        value: "target",
        label: "The remote's own size",
        note: "This target is shown at the size its server has.",
      },
    ];
  }
  // The default says so in its label, where it is seen without asking.
  const kept = (
    size: Points,
    value: Sizing,
    fallback: boolean,
  ): SizeOption => ({
    value,
    label: `${size.w}×${size.h}${fallback ? " (default)" : ""}`,
    note: `${fallback ? "The default size." : "The size set for this target."} The desktop stays at it.`,
  });
  const configured = target.size ? [kept(target.size, "target", false)] : [];
  if (target.resize && follows) {
    return [...configured, FOLLOWS[follows]];
  }
  if (target.size === null) {
    return [kept(target.defaultSize, "target", true)];
  }
  return target.resize
    ? [...configured, kept(target.defaultSize, "default", true)]
    : configured;
}

/**
 * `target`'s sound as the picker shows it: nothing where it offers none, and
 * otherwise a tick and, under a ticked one, Opus or FLAC. Ticking it takes
 * Opus. A browser whose decoder does not play FLAC greys it with the reason,
 * and one with no audio decoder has nothing to tick. A format remembered from
 * before that does not play here is not sent, and stays remembered for a
 * browser that plays it.
 */
function soundRow(
  target: TargetInfo,
  remembered: Sound | undefined,
  plays: boolean,
  lossless: boolean,
): { row: SoundRow | null; audio: Sound } {
  if (!target.audio) {
    return { row: null, audio: "off" };
  }
  const formats: SoundFormat[] = [
    {
      value: "opus",
      label: "Opus",
      note: !plays
        ? "This browser has no WebCodecs AudioDecoder to play it with."
        : "Compressed, up to 320 kbit/s, lower while the link is behind.",
      disabled: !plays,
    },
    {
      value: "flac",
      label: "FLAC",
      note: lossless
        ? "Lossless, about a megabit a second of music. For a LAN."
        : !plays
          ? "This browser has no WebCodecs AudioDecoder to play it with."
          : "This browser's AudioDecoder does not decode FLAC.",
      disabled: !lossless,
    },
  ];
  // What was chosen last time, where it plays here.
  const audio =
    formats.find((format) => format.value === remembered && !format.disabled)
      ?.value ?? "off";
  return {
    row: {
      label: "Sound",
      note: plays
        ? "Take the remote's sound and play it here."
        : "This browser has no WebCodecs AudioDecoder, so it cannot play the remote's sound.",
      checked: audio !== "off",
      disabled: !plays,
      formats,
    },
    audio,
  };
}

// This browser's devices, offered to the remote where the target has somewhere to
// put them: an RDP host over its channels, wlshare over its extensions. The tick
// is the offer; the session's menu turns the device on, and off.
const DEVICES: Record<"camera" | "microphone", Omit<DeviceRow, "checked">> = {
  camera: {
    label: "Camera (experimental)",
    note: "Offer the remote this browser's camera, as H.264, to turn on from the session's menu. For a call.",
  },
  microphone: {
    label: "Microphone (experimental)",
    note: "Offer the remote this browser's microphone, as speech Opus, to turn on from the session's menu. For a call or a recording.",
  },
};

/**
 * `target`'s `device` as the picker shows it: nothing where it has nowhere to put
 * it, and otherwise a tick, off until somebody ticks it.
 */
function deviceRow(
  target: TargetInfo,
  device: "camera" | "microphone",
  remembered: boolean | undefined,
): { row: DeviceRow | null; offered: boolean } {
  if (!target[device]) {
    return { row: null, offered: false };
  }
  const offered = remembered === true;
  return { row: { ...DEVICES[device], checked: offered }, offered };
}

// A Standard Mac's screens: one at a time, the Mac's own picture passed, or every
// screen at once as Combined Display, which the gateway codes from the Mac's Full
// quality, a screen denser than this display scaled down to it there.
const VIEWS: ViewOption[] = [
  {
    value: "one",
    label: "One at a time (beta)",
    note: "The main screen first, the others in the display menu, in the picture chosen below.",
  },
  {
    value: "combined",
    label: "Combined (alpha)",
    note: "Every screen at once, as Apple's viewer's Combined Display: the Mac's Full quality, sent as tiles by the gateway, which fit it to the link. Best for two screens that fit one window.",
  },
];

// Why a Standard Mac's own picture is no choice under Combined Display.
const COMBINED_PICTURE =
  "Not with Combined Display, which is the gateway's tiles of every screen.";

// The second display against the first: the edge of the first a window dragged
// over arrives on the second from. The right is where it has always been.
const PLACEMENTS: PlacementOption[] = [
  { value: "right", label: "Right" },
  { value: "left", label: "Left" },
  { value: "top", label: "Top" },
  { value: "bottom", label: "Bottom" },
];

/**
 * `target`'s pipeline as the picker shows it: nothing where it is no choice, and
 * otherwise high quality or adaptive, adaptive's H.264 over the whole desktop by
 * default or by region. A browser that cannot decode the H.264 has adaptive
 * greyed; an adaptive one remembered from before is not sent, and stays
 * remembered for a browser that can.
 */
function pipelineRow(
  target: TargetInfo,
  remembered: Pipeline | undefined,
  decodes: boolean,
): { row: PipelineRow | null; pipeline: Pipeline } {
  if (!target.pipeline) {
    return { row: null, pipeline: "high-quality" };
  }
  const h264: PipelineRow["h264"] = [
    {
      value: "adaptive-desktop",
      label: "H.264 full screen",
      note: "The whole desktop in H.264, which the host fits to the link.",
      disabled: false,
    },
    {
      value: "adaptive-region",
      label: "H.264 region",
      note: "H.264 only where the desktop moves like video, the rest as High quality draws it, which the host does not fit to the link.",
      disabled: false,
    },
  ];
  const pipeline =
    remembered !== undefined &&
    (remembered === "high-quality" ||
      (decodes && h264.some((option) => option.value === remembered)))
      ? remembered
      : "high-quality";
  return {
    row: {
      modes: [
        {
          value: "high-quality",
          label: "High quality",
          note: "The pipeline's codecs without H.264. For a LAN.",
          disabled: false,
        },
        {
          value: "adaptive",
          label: "Adaptive (alpha)",
          note: decodes
            ? "H.264, which the host fits to the link this browser is on."
            : "This browser has no VideoDecoder that takes the host's H.264.",
          disabled: !decodes,
        },
      ],
      h264,
    },
    pipeline,
  };
}

/**
 * `target`'s screens and picture as the picker shows them: one at a time or
 * Combined Display on a Standard Mac, the remembered one where it is offered, and
 * the Mac's pictures, the passed one greyed under Combined Display, which is the
 * gateway's tiles.
 */
function viewRows(
  target: TargetInfo,
  remembered: StandardView | undefined,
  abilities: Abilities,
): {
  views: ViewOption[] | null;
  standardView: StandardView;
  pictures: PictureOption[] | null;
} {
  const views = target.standardView ? VIEWS : null;
  const standardView =
    views?.find((option) => option.value === remembered)?.value ?? "one";
  const offered = target.passthroughs.some(
    (passthrough) => passthrough !== "rdp-graphics",
  )
    ? pictureOptions(target, abilities)
    : null;
  const pictures =
    standardView === "combined" && offered
      ? offered.map((option) =>
          option.value === null
            ? option
            : { ...option, note: COMBINED_PICTURE, disabled: true },
        )
      : offered;
  return { views, standardView, pictures };
}

/**
 * The options under `target`, from what was `remembered` for it and what this
 * browser can do. A size the operator configured is the size until somebody
 * chooses another, and no remote's sound starts playing until somebody asks.
 */
export function targetOptions(
  target: TargetInfo,
  remembered: Partial<Choices> | undefined,
  abilities: Abilities,
): TargetOptions {
  const sizes = sizeOptions(target, abilities.follows);
  const size =
    sizes.find((option) => option.value === remembered?.size)?.value ??
    sizes[0].value;
  const sound = soundRow(
    target,
    remembered?.audio,
    abilities.audio,
    abilities.lossless,
  );
  const drawn = pipelineRow(target, remembered?.pipeline, abilities.rdpH264);
  const camera = deviceRow(target, "camera", remembered?.camera);
  const microphone = deviceRow(target, "microphone", remembered?.microphone);
  // An RDP host's pipeline is passed whatever is chosen.
  const always = target.passthroughs.includes("rdp-graphics");
  const passed: PassedNote | null = always
    ? { label: RDP_GRAPHICS.label, note: RDP_GRAPHICS.note }
    : null;
  const { views, standardView, pictures } = viewRows(
    target,
    remembered?.standardView,
    abilities,
  );
  // What was chosen last time, where it can be had here, and otherwise the
  // first that can: a Standard Mac's pictures always can. A remembered null is
  // the gateway's tiles where those are a choice, and nothing chosen anywhere
  // else.
  const able = pictures?.filter((option) => !option.disabled) ?? [];
  const passthrough =
    (
      able.find(
        (option) =>
          remembered !== undefined &&
          "passthrough" in remembered &&
          option.value === remembered.passthrough,
      ) ?? able[0]
    )?.value ?? null;
  const placements = target.placement ? PLACEMENTS : null;
  const choices: Choices = {
    size,
    audio: sound.audio,
    passthrough,
    placement:
      placements?.find((option) => option.value === remembered?.placement)
        ?.value ?? "right",
    pipeline: drawn.pipeline,
    standardView,
    camera: camera.offered,
    microphone: microphone.offered,
  };
  return {
    sizes,
    soundRow: sound.row,
    cameraRow: camera.row,
    microphoneRow: microphone.row,
    placements,
    pipelineRow: drawn.row,
    views,
    passed,
    pictures,
    choices,
    // High Performance's sound comes with its picture, so it has no row and is
    // always there, for a browser that can play it.
    sound:
      choices.audio !== "off" ||
      (target.passthroughs.includes("apple-media") && abilities.audio),
    blocked: blockedReason(target, abilities, able.length > 0),
  };
}

/**
 * Why `target` cannot start here, or null: its only pictures are ones this
 * browser cannot take. An RDP host's pipeline in a page that cannot compose it,
 * a High Performance Mac with no picture `pictured` here, and a `wlshare`
 * target's pictures, VP9 both, in a browser whose decoder refuses VP9.
 */
function blockedReason(
  target: TargetInfo,
  abilities: Abilities,
  pictured: boolean,
): string | null {
  if (target.passthroughs.includes("rdp-graphics") && !abilities.rdpGraphics) {
    return RDP_GRAPHICS.blocked;
  }
  if (target.passthroughs.includes("apple-media") && !pictured) {
    return NO_APPLE_PICTURE;
  }
  if (
    target.protocol === "vnc" &&
    target.subtype === "wlshare" &&
    !abilities.vp9
  ) {
    return NO_VP9;
  }
  return null;
}

// Remembered per target, in this browser, as lasting choices about how each
// desktop is used from this machine: localStorage rather than sessionStorage, so
// they survive a new tab.
const CHOICES_KEY = "zdesk.targetChoices";

/** Every target's remembered choices, by name. Empty where nothing can be read. */
export function readRememberedChoices(): Record<string, Partial<Choices>> {
  try {
    const stored: unknown = JSON.parse(
      localStorage.getItem(CHOICES_KEY) ?? "{}",
    );
    return stored !== null && typeof stored === "object"
      ? (stored as Record<string, Partial<Choices>>)
      : {};
  } catch {
    return {}; // storage disabled or blocked, or not what this wrote
  }
}

/**
 * `remembered` with `key` set for `target`, written back. Only a choice somebody
 * could change is ever remembered: a greyed row says what this browser or this
 * gateway can do, which is not a choice to carry to another.
 */
export function rememberChoice<K extends keyof Choices>(
  remembered: Record<string, Partial<Choices>>,
  target: string,
  key: K,
  value: Choices[K],
): Record<string, Partial<Choices>> {
  const next = {
    ...remembered,
    [target]: { ...remembered[target], [key]: value },
  };
  try {
    localStorage.setItem(CHOICES_KEY, JSON.stringify(next));
  } catch {
    // Not persisted; the choice still holds for this page.
  }
  return next;
}
