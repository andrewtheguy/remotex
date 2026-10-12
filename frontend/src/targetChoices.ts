// What a session is started with: how the desktop is sized, whether the remote's
// sound is taken and as what, whether the target's own stream is passed, and where
// a second virtual display sits. Chosen under the target at the picker, before
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
//   a browser with no audio decoder, a passthrough this browser cannot take, or
//   one that is the only way this gateway can serve the target, which is then
//   shown ticked. An RDP host's graphics pipeline is always that: the gateway
//   composes none.
// - Sound is ticked or not where the target offers it, and a ticked one is Opus
//   or lossless: see `soundRow`. In a browser with no audio decoder only the
//   lossless one plays, which the page decodes itself, so ticking takes it.
// - Where the second display sits is a choice on a target that asks its host for
//   two and tells it where each is: see `PLACEMENTS`.
// - How the host draws an RDP pipeline, lossless or adaptive H.264 (alpha), is a
//   choice on a target with the pipeline on, and under adaptive the whole desktop
//   or by region: see `pipelineRow`. A browser that cannot decode the H.264 is
//   offered lossless alone.
// - A target that can only be passed, in a browser that cannot take it, cannot
//   start, and Start says so before the remote is dialled.
// - Which decoder takes the picture is no choice: the page decodes the VP9, and
//   BETA a Mac's passed HEVC where the gateway serves the decoder for it, in
//   WebAssembly wherever the browser's own decoder does not take it
//   (softwareDecoder.ts).

/** The stream a target can pass untouched, as `/api/targets` names it. */
export type Passthrough = "rdp-graphics" | "apple-media";

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
   * BETA: whether a page whose own decoder refuses this target's passed HEVC can
   * decode it in the software decoder the gateway serves.
   */
  software: boolean;
  /** The stream this target can pass, null where it has none. */
  passthrough: Passthrough | null;
  /**
   * Whether passing it is the only way this gateway can serve the target: an RDP
   * host's graphics pipeline, which no gateway composes, and a High Performance
   * Mac on a host that cannot decode its stream.
   */
  passthroughOnly: boolean;
  /** Whether where the second virtual display sits is a choice. */
  placement: boolean;
  /** Whether how the host draws the graphics pipeline is a choice. */
  pipeline: boolean;
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
 * with its lossless codecs, or adaptive, in H.264 the host fits to the link, over
 * the whole desktop or only where it moves like video.
 */
export type Pipeline = "lossless" | "adaptive-desktop" | "adaptive-region";

/** What `connect` carries. */
export interface Choices {
  size: Sizing;
  audio: Sound;
  passthrough: boolean;
  placement: Placement;
  pipeline: Pipeline;
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
   * Whether this browser has an audio decoder, which Opus and a Mac's AAC-ELD
   * need. FLAC does not: the page decodes it itself (flacDecoder.ts).
   */
  audio: boolean;
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
  /** What ticking it takes: the first format this browser plays. */
  ticks: Exclude<Sound, "off">;
  /** Opus first, then lossless. */
  formats: SoundFormat[];
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
 * How the host draws the pipeline: lossless or adaptive, and under an adaptive
 * one, the whole desktop or by region.
 */
export interface PipelineRow {
  modes: PipelineOption<"lossless" | "adaptive">[];
  h264: PipelineOption<Exclude<Pipeline, "lossless">>[];
}

/** One place the second display can sit. */
export interface PlacementOption {
  value: Placement;
  label: string;
}

/** One option under an open target. */
export interface OptionRow {
  key: "passthrough";
  label: string;
  /** What ticking it does, or why it cannot be changed here. */
  note: string;
  checked: boolean;
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
  /**
   * Where the second display can sit, on a target whose host is told: null
   * where it is not a choice.
   */
  placements: PlacementOption[] | null;
  /** How the host draws the pipeline, as a choice: null where it is none. */
  pipelineRow: PipelineRow | null;
  rows: OptionRow[];
  /** What Start sends. */
  choices: Choices;
  /** Whether a session started with them carries the remote's sound. */
  sound: boolean;
  /** Why the target cannot start in this browser, or null. */
  blocked: string | null;
}

const PASSTHROUGH: Record<
  Passthrough,
  {
    label: string;
    // What is said where the stream can only be passed.
    only: { note: string; blocked: string };
    // What is said where passing it is a choice, null for a stream that is
    // passed in every session.
    chosen: { note: string; cannot: string } | null;
  }
> = {
  "rdp-graphics": {
    label: "The graphics pipeline, passed through",
    only: {
      note: "The host's drawing is composed in this browser, in every session.",
      blocked:
        "This browser cannot compose the host's graphics pipeline, which this target always passes: that needs WebGL 2 and a cross-origin isolated page. Use a browser that can, or a target with egfx disabled.",
    },
    chosen: null,
  },
  "apple-media": {
    label: "Pass the Mac's picture through",
    only: {
      note: "This gateway cannot decode the Mac's picture, so it is always passed.",
      blocked:
        "This gateway cannot decode the Mac's picture, and this browser cannot take it passed through. Use a browser that decodes it, or install FFmpeg on the gateway's host.",
    },
    chosen: {
      note: "The Mac's own HEVC, instead of VP9 encoded by the gateway. For a LAN.",
      cannot:
        "This browser does not decode the Mac's HEVC, and this page cannot decode it here.",
    },
  },
};

/**
 * Whether this browser can be passed `target`'s stream: where its own decoder
 * takes it, and a Mac's HEVC also where the page decodes it itself, which it then
 * does (softwareDecoder.ts). The socket states both answers about the HEVC, and
 * the gateway passes the picture to a page that has either.
 */
function takes(
  target: TargetInfo,
  abilities: Abilities,
  passthrough: Passthrough,
): boolean {
  if (passthrough === "rdp-graphics") {
    return abilities.rdpGraphics;
  }
  return abilities.appleMedia || (target.software && abilities.runsHevc);
}

/**
 * The passthrough row of a target that has a stream to pass, and why the target
 * cannot start here where that stream is the only way and this browser cannot
 * take it.
 */
function passthroughRow(
  target: TargetInfo,
  passthrough: Passthrough,
  remembered: boolean | undefined,
  abilities: Abilities,
): { row: OptionRow; blocked: string | null } {
  const words = PASSTHROUGH[passthrough];
  const able = takes(target, abilities, passthrough);
  const row = { key: "passthrough" as const, label: words.label };
  const chosen = target.passthroughOnly ? null : words.chosen;
  if (!chosen) {
    return {
      row: { ...row, note: words.only.note, checked: true, disabled: true },
      blocked: able ? null : words.only.blocked,
    };
  }
  if (!able) {
    return {
      row: { ...row, note: chosen.cannot, checked: false, disabled: true },
      blocked: null,
    };
  }
  return {
    row: {
      ...row,
      note: chosen.note,
      checked: remembered ?? false,
      disabled: false,
    },
    blocked: null,
  };
}

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
        note: "This target shares its displays as they are.",
      },
    ];
  }
  // A plain VNC server is asked, and whether it takes a size is known only once
  // it is dialled.
  const asked =
    target.protocol === "vnc" && target.subtype === null
      ? " A server that takes no size keeps its own."
      : "";
  const kept = (size: Points, value: Sizing, what: string): SizeOption => ({
    value,
    label: `${size.w}×${size.h}`,
    note: `${what} The desktop stays at it.${asked}`,
  });
  const configured = target.size
    ? [kept(target.size, "target", "The size set for this target.")]
    : [];
  if (target.resize && follows) {
    return [...configured, FOLLOWS[follows]];
  }
  if (target.size === null) {
    return [kept(target.defaultSize, "target", "The default size.")];
  }
  return target.resize
    ? [...configured, kept(target.defaultSize, "default", "The default size.")]
    : configured;
}

/**
 * `target`'s sound as the picker shows it: nothing where it offers none, and
 * otherwise a tick and, under a ticked one, Opus or FLAC. Ticking it takes
 * Opus. A browser with no audio decoder plays FLAC alone, so there Opus is
 * greyed and ticking takes FLAC; an Opus remembered from before is not sent,
 * and stays remembered for a browser that can.
 */
function soundRow(
  target: TargetInfo,
  remembered: Sound | undefined,
  plays: boolean,
): { row: SoundRow | null; audio: Sound } {
  if (!target.audio) {
    return { row: null, audio: "off" };
  }
  const formats: SoundFormat[] = [
    {
      value: "opus",
      label: "Opus",
      note: plays
        ? "Compressed, at a rate that follows the link."
        : "This browser has no WebCodecs AudioDecoder to play it with.",
      disabled: !plays,
    },
    {
      value: "flac",
      label: "FLAC",
      note: "Lossless, about a megabit a second of music. For a LAN.",
      disabled: false,
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
        : "Take the remote's sound and play it here, lossless: this browser has no decoder for Opus.",
      checked: audio !== "off",
      ticks: plays ? "opus" : "flac",
      formats,
    },
    audio,
  };
}

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
 * otherwise lossless or adaptive, adaptive's H.264 over the whole desktop by
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
    return { row: null, pipeline: "lossless" };
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
      note: "H.264 only where the desktop moves like video, the rest lossless, which the host does not fit to the link.",
      disabled: false,
    },
  ];
  const pipeline =
    remembered !== undefined &&
    (remembered === "lossless" ||
      (decodes && h264.some((option) => option.value === remembered)))
      ? remembered
      : "lossless";
  return {
    row: {
      modes: [
        {
          value: "lossless",
          label: "Lossless",
          note: "The host's lossless codecs alone. For a LAN.",
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
  const sound = soundRow(target, remembered?.audio, abilities.audio);
  const drawn = pipelineRow(target, remembered?.pipeline, abilities.rdpH264);
  const rows: OptionRow[] = [];
  let blocked: string | null = null;
  if (target.passthrough) {
    const passed = passthroughRow(
      target,
      target.passthrough,
      remembered?.passthrough,
      abilities,
    );
    rows.push(passed.row);
    blocked = passed.blocked;
  }
  const passthrough = rows.some((row) => row.checked);
  const placements = target.placement ? PLACEMENTS : null;
  const choices: Choices = {
    size,
    audio: sound.audio,
    passthrough,
    placement:
      placements?.find((option) => option.value === remembered?.placement)
        ?.value ?? "right",
    pipeline: drawn.pipeline,
  };
  return {
    sizes,
    soundRow: sound.row,
    placements,
    pipelineRow: drawn.row,
    rows,
    choices,
    // High Performance's sound comes with its picture, so it has no row and is
    // always there, for a browser that can play it.
    sound:
      choices.audio !== "off" ||
      (target.passthrough === "apple-media" && abilities.audio),
    blocked,
  };
}

// Remembered per target, in this browser, as lasting choices about how each
// desktop is used from this machine: localStorage rather than sessionStorage, so
// they survive a new tab.
const CHOICES_KEY = "remotex.targetChoices";

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
