import {
  createRelaySink,
  createRelaySource,
  openRelayPort,
  type RelayPort,
  type RelaySink,
  type RelaySource,
} from "./displayRelay.ts";
import {
  type EgfxCompositor,
  type EgfxFactory,
  loadEgfx,
} from "./egfxCompositor.ts";
import type { GraphicsPicture, PicturePart } from "./egfxPicture.ts";
import { createEgfxVideo, type EgfxVideo } from "./egfxVideo.ts";
import type { PlanesPicture } from "./planesPicture.ts";
import {
  type BatchRecord,
  decodeBatchFrame,
  type GraphicsMsg,
  type VideoMsg,
} from "./protocol.ts";
import {
  type DecodedPicture,
  isSoftwarePlanes,
  type SoftwareModule,
  type SoftwareRefusals,
} from "./softwareDecoder.ts";
import {
  createDesktopVideo,
  type DesktopVideo,
  type VideoFormat,
} from "./videoDecoder.ts";

// The browser SPA's batch draw loop: each batch's records — access units and runs
// of an RDP host's graphics pipeline — decoded or composed in wire order and
// drawn onto the canvas. The decoder lives here rather than beside each caller:
// it belongs to exactly one attachment, and `clear` is the one place that ends it.

// The destination's 2D context. A union rather than the element's alone because
// the painter runs inside the paint worker, drawing through an `OffscreenCanvas` —
// the element context remains for the unit tests, which drive the painter directly.
export type PaintContext =
  | CanvasRenderingContext2D
  | OffscreenCanvasRenderingContext2D;

export interface FramePainter {
  /**
   * Decode one binary batch frame and paint it, in wire order. Malformed framing
   * drops the batch, which cuts the stream's chain, so it also asks for a keyframe.
   */
  draw(frame: ArrayBuffer): Promise<void>;
  /**
   * Drop the decoder. The next attachment's stream starts again from a keyframe.
   */
  clear(): void;
  /**
   * Adopt a `videoFormat`: the exact string to configure the decoder with. Always
   * arrives before the stream's first access unit.
   *
   * Held here rather than passed with each unit because it is announced once and used
   * by every unit after it. A runtime that fails here says so through `onVideoError`
   * exactly as a failing decode does.
   */
  setVideoFormat(format: VideoFormat): void;
  /**
   * Adopt a `graphicsStart`: an RDP host's graphics pipeline begins, from nothing,
   * and the GRAPHICS records after it are the picture. Whatever decoder or
   * compositor stood before is done with.
   */
  startGraphics(): void;
  /**
   * Adopt a `graphicsView` on the page that composes the pipeline: the part of
   * its picture this display is, shown from now, by this pipeline and the next.
   */
  setGraphicsView(part: PicturePart): void;
  /**
   * Adopt a `graphicsView` in a tab showing display `display` of its own: the
   * part of the session page's picture the tab is painted from
   * (displayRelay.ts). The tab composes nothing.
   */
  mirrorGraphics(display: number, part: PicturePart): void;
  /**
   * The desktop's canvas was replaced at this size and filled black. A pipeline's
   * picture is shown over that canvas, so its canvas is blanked with it, and what
   * it holds is kept for the `graphicsView` that follows: over a span, the resize
   * is the picker's switch between displays. A software decoder's is no
   * longer shown, until the stream's next picture.
   */
  blank(w: number, h: number): void;
}

export function createFramePainter(options: {
  /**
   * The destination, read per batch rather than captured, so a resize that
   * replaces the 2D context does not need the painter rebuilt.
   */
  context: () => PaintContext | null;
  /**
   * Why this client is showing nothing, or null once it is showing something. This
   * cannot be swallowed: the stream is all a target sends, so the alternative to
   * saying it is a desktop that never paints and never explains itself.
   */
  onVideoError: (reason: string | null) => void;
  /**
   * The stream's chain has been cut — its decoder went quiet, or it failed and was
   * thrown away — so the desktop cannot paint again until a keyframe only the gateway
   * can send. Separate from `onVideoError` because it asks for something rather than
   * saying something: it is the recovery, where the banner is the report, and the two
   * are answered in different places.
   */
  onVideoNeedsKeyframe: (reason: string) => void;
  /**
   * Where compositors come from: the WebAssembly module, loaded once. Injectable
   * for a test, which has the module's bytes and nothing to fetch them from.
   */
  loadCompositor?: () => Promise<EgfxFactory>;
  /**
   * Where a pipeline's picture comes from: the WebGL canvas the page shows it on
   * (egfxPicture.ts). A painter given none composes no pipeline.
   */
  makePicture?: () => GraphicsPicture;
  /**
   * The channel the second display's picture crosses the browser on
   * (displayRelay.ts), opened by the page that composes it and by the tab that
   * shows it. Injectable for a test, which has no BroadcastChannel to open.
   */
  makeRelay?: () => RelayPort;
  /**
   * EXPERIMENTAL: what decodes the H.264 a pipeline may carry (egfxVideo.ts): the
   * browser's `VideoDecoder`, a stream for each surface. Injectable for a test,
   * which has no decoder.
   */
  makeGraphicsVideo?: () => EgfxVideo;
  /**
   * BETA: where a software decoder's pictures are drawn, the same canvas
   * (planesPicture.ts). A painter given none presents none.
   */
  makePlanesPicture?: () => PlanesPicture;
  /**
   * Whether that canvas holds what the page should be showing: true once a
   * pipeline has drawn its first run or the software decoder a picture, false
   * when the desktop's own canvas is the picture again. It lies over the
   * desktop's, and only the page can show or hide it.
   */
  onGraphicsShown?: (shown: boolean) => void;
  /** BETA: the streams decoded in software (see `createDesktopVideo`). */
  software?: readonly SoftwareModule[];
  /** BETA: the streams refused, and why (see `createDesktopVideo`). */
  refused?: SoftwareRefusals;
}): FramePainter {
  // Which attachment the decoder belongs to. `clear()` is the attachment boundary and
  // is not queued behind draws — an eviction closes the socket from under whatever
  // batch is mid-decode — so a draw that outlives the generation it started in must
  // not paint onto the next one.
  let generation = 0;

  // The decoder, built on the first announcement or unit.
  let video: DesktopVideo | null = null;

  // The pipeline being composed, from its `graphicsStart`. `compositor` and
  // `picture` are null until the module has loaded, and for good once `broken`: a
  // compositor that refused a command no longer holds what the host believes its
  // client does, and nothing composed from it afterwards could be trusted.
  interface Pipeline {
    compositor: EgfxCompositor | null;
    picture: GraphicsPicture | null;
    /** Its H.264 decoders, made by the first access unit it carries. */
    video: EgfxVideo | null;
    /** Whether the page has been told to show the picture. */
    shown: boolean;
    broken: boolean;
    ready: Promise<void>;
  }
  let pipeline: Pipeline | null = null;
  const loadCompositor = options.loadCompositor ?? (() => loadEgfx());
  const makeGraphicsVideo = options.makeGraphicsVideo ?? createEgfxVideo;
  const makePicture =
    options.makePicture ??
    (() => {
      throw new Error("the page gave no canvas for the pipeline's picture");
    });
  const makeRelay = options.makeRelay ?? openRelayPort;

  // The part of the picture this page's display is, from the gateway's last word:
  // the whole of it until told, which is every pipeline the host draws over one
  // display. Kept across pipelines, since the host's layout is not theirs.
  let graphicsPart: PicturePart | null = null;
  // The second display's end of displayRelay.ts on the page that composes: made
  // with the first pipeline, told what every run paints, and kept for the page's
  // life — the tab may open before a pipeline or outlive one.
  let source: RelaySource | null = null;
  // The other end, in a tab showing a display of its own: its picture, and
  // whether the page has been told to show it.
  interface Mirror {
    sink: RelaySink;
    picture: GraphicsPicture;
    shown: boolean;
  }
  let mirror: Mirror | null = null;

  // A pipeline's compositor, given back; it composes nothing more. Its picture
  // stays where it is, showing what was last drawn.
  const stop = (done: Pipeline) => {
    done.broken = true;
    // Settles the unit a run is waiting on, which is what lets a run that this
    // ended in mid-decode come back and find it ended.
    done.video?.close();
    done.video = null;
    done.compositor?.close();
    done.compositor = null;
  };

  // Whatever a pipeline holds, given back, and its picture no longer shown.
  const finish = (done: Pipeline) => {
    stop(done);
    done.picture?.close();
    done.picture = null;
    if (done.shown) {
      done.shown = false;
      options.onGraphicsShown?.(false);
    }
  };

  const releasePipeline = () => {
    if (pipeline) {
      finish(pipeline);
    }
    pipeline = null;
    source?.reset();
  };

  // A tab's mirror, given back, and its picture no longer shown.
  const releaseMirror = () => {
    if (!mirror) {
      return;
    }
    const done = mirror;
    mirror = null;
    done.sink.close();
    done.picture.close();
    if (done.shown) {
      options.onGraphicsShown?.(false);
    }
  };

  const describe = (error: unknown) =>
    error instanceof Error ? error.message : String(error);

  // A software decoder's picture, made by the first one it decodes. A Mac's
  // passed stream gives way to VP9 encoded here across a display change, which
  // the browser's decoder may take and is then painted on the desktop's own
  // canvas, so `shown` goes both ways in a session.
  // `broken` once the GPU would not take a picture: said once, and the pictures
  // after it are dropped, since nothing sent again would be taken either.
  const planar: {
    picture: PlanesPicture | null;
    shown: boolean;
    broken: boolean;
  } = { picture: null, shown: false, broken: false };

  const hidePlanes = () => {
    if (planar.shown) {
      planar.shown = false;
      options.onGraphicsShown?.(false);
    }
  };

  const releasePlanes = () => {
    hidePlanes();
    planar.picture?.close();
    planar.picture = null;
    planar.broken = false;
  };

  // What is on screen about video, and whether a painted frame may take it down.
  //
  // A decoder giving up is answered by a painted frame: what the banner says is that
  // this client is showing nothing, so a frame is it ceasing to be true. A refusal is
  // not: the browser will not take that configuration, and nothing painted under it
  // says otherwise — the banner stays until the attachment ends or the gateway
  // announces a different configuration. That can happen: the announced VP9 level
  // follows the desktop's size, so a browser that refused a large picture may take
  // the smaller one a resize brings, and then its first painted frame is the answer.
  let videoComplained = false;
  // The configuration that was refused, or null.
  let refused: string | null = null;

  const complainAboutVideo = (
    reason: string,
    recoverable: boolean,
    decode: string,
  ) => {
    if (refused !== null && recoverable) {
      // The standing fact is the more useful sentence and it is already up.
      return;
    }
    if (!recoverable) {
      refused = decode;
    }
    videoComplained = recoverable;
    options.onVideoError(reason);
  };

  const releaseVideo = () => {
    releasePipeline();
    releaseMirror();
    releasePlanes();
    video?.close();
    video = null;
    videoComplained = false;
    refused = null;
    // Retracted, and not merely forgotten. This is the attachment boundary: the
    // decoder that said it is gone, the next attachment may be a different target
    // through a different origin, and the page clears its own copy on the way back to
    // the picker only — a reattach would otherwise inherit the sentence.
    options.onVideoError(null);
  };

  const desktopVideo = (): DesktopVideo => {
    if (video) {
      return video;
    }
    // Rebuilding a failed decoder is not this client's decision: the stream begins
    // again when the gateway sends a keyframe, which a repaint or a resize does.
    video = createDesktopVideo(
      {
        onError: complainAboutVideo,
        onNeedsKeyframe: (reason) => options.onVideoNeedsKeyframe(reason),
      },
      undefined,
      options.software,
      options.refused,
    );
    videoComplained = false;
    options.onVideoError(null);
    return video;
  };

  // The end of a pipeline this page can no longer follow. Its compositor holds what
  // the host believes its client does only while it has composed every command, so
  // one that refused a command or was never given one is not fed again; and nothing
  // is asked of the gateway, since a host answers a repaint out of the caches this
  // compositor no longer has. Said once: what follows is the same fact. The picture
  // is kept, as the desktop under the sentence, until the pipeline is replaced.
  const endPipeline = (broken: Pipeline, why: string) => {
    if (broken.broken) {
      return;
    }
    stop(broken);
    videoComplained = false;
    options.onVideoError(
      `This browser could not compose the host's graphics (${why}). Reload the page to start the session over.`,
    );
  };

  // Every unit is part of one chain, so a dropped batch cuts it: the deltas after it
  // name a picture this decoder never made. Restarted rather than fed them, and a
  // keyframe asked for, exactly as a failed decoder is. A pipeline's dropped batch
  // is commands its
  // compositor never composed, which no repaint brings back, so it ends there.
  const dropMalformed = () => {
    if (pipeline) {
      endPipeline(pipeline, "a batch of its commands arrived malformed");
      return;
    }
    video?.restart();
    options.onVideoNeedsKeyframe("a malformed batch was dropped");
  };

  // A run's H.264, where it has any, ahead of composing it: each access unit
  // through its surface's decoder in command order, and the picture handed to the
  // compositor, which paints it where the run's commands say. Every wait is one
  // the pipeline may end during, and an ended one's compositor is not to be
  // touched: false then, and the run is dropped.
  const decodeUnits = async (
    current: Pipeline,
    compositor: EgfxCompositor,
    commands: Uint8Array,
  ): Promise<boolean> => {
    for (const found of compositor.scan(commands)) {
      if ("gone" in found) {
        current.video?.drop(found.gone);
        continue;
      }
      const { unit, number } = found;
      current.video ??= makeGraphicsVideo();
      const frame = await current.video.decode(
        unit,
        commands.subarray(unit.start, unit.end),
      );
      try {
        if (!current.broken) {
          await compositor.supply(number, unit.window, frame);
        }
      } finally {
        frame.close();
      }
      if (current.broken) {
        return false;
      }
    }
    return true;
  };

  // One run of the pipeline, composed and painted when its turn comes. Everything a
  // run needs is what the runs before it left, so nothing here starts early: the
  // module's load is waited for in place, and a run for a pipeline that has been
  // replaced, broken or never started is dropped.
  const compose = async (record: GraphicsMsg, born: number) => {
    const current = pipeline;
    if (!current) {
      return;
    }
    await current.ready;
    const { compositor, picture } = current;
    if (
      generation !== born ||
      current !== pipeline ||
      !compositor ||
      !picture
    ) {
      return;
    }
    let run: ReturnType<EgfxCompositor["compose"]>;
    try {
      if (!(await decodeUnits(current, compositor, record.data))) {
        return;
      }
      run = compositor.compose(record.data);
      picture.upload(run);
    } catch (error) {
      endPipeline(current, describe(error));
      return;
    }
    // And the tab's share of it, out of the same picture.
    source?.painted(run);
    // Shown from its first drawn run, and not before: until then the picture's
    // canvas holds nothing of this pipeline's.
    if (!current.shown && run.width > 0 && run.height > 0) {
      current.shown = true;
      options.onGraphicsShown?.(true);
    }
  };

  // One unit's picture. Null when there is nothing to draw — a decoder that dropped
  // the unit has said so itself. A run of the pipeline has no picture of its own: it
  // is composed in its turn.
  const decode = (record: BatchRecord): Promise<DecodedPicture | null> => {
    if (record.kind === "graphics") {
      return Promise.resolve(null);
    }
    return desktopVideo().decode(
      { w: record.w, h: record.h },
      record.data,
      record.keyframe,
    );
  };

  // One of a software decoder's pictures, onto the canvas over the desktop's.
  // False when the GPU would not take it, which is said and ends the presenting.
  const presentPlanes = (
    planes: Parameters<PlanesPicture["draw"]>[0],
    w: number,
    h: number,
  ): boolean => {
    if (planar.broken || !options.makePlanesPicture) {
      return false;
    }
    try {
      planar.picture ??= options.makePlanesPicture();
      planar.picture.draw(planes, w, h);
    } catch (error) {
      hidePlanes();
      planar.picture?.close();
      planar.picture = null;
      planar.broken = true;
      videoComplained = false;
      options.onVideoError(
        `This browser could not present the decoded picture (${describe(error)}). Reload the page to start the session over.`,
      );
      return false;
    }
    if (!planar.shown) {
      planar.shown = true;
      options.onGraphicsShown?.(true);
    }
    return true;
  };

  const paint = (record: VideoMsg, image: DecodedPicture) => {
    const context = options.context();
    if (isSoftwarePlanes(image)) {
      if (!presentPlanes(image, record.w, record.h)) {
        return;
      }
    } else {
      const { w, h } = record;
      // Cropped by the desktop's size rather than drawn whole: the encoder is held
      // to even sides and an odd desktop does not have them, so the decoded picture
      // can be a pixel wider or taller than the desktop.
      context?.drawImage(image, 0, 0, w, h, 0, 0, w, h);
      // The desktop's own canvas is the picture again.
      hidePlanes();
    }
    if (videoComplained) {
      // Video is painting again, so whatever was said about it has stopped being
      // true. Said here rather than on a timer or behind a dismiss button: the
      // banner is a statement about the present, and this is the moment the
      // present changed.
      videoComplained = false;
      options.onVideoError(null);
    }
  };

  return {
    async draw(frame: ArrayBuffer) {
      const records = decodeBatchFrame(frame);
      if (!records) {
        dropMalformed();
        return;
      }
      const born = generation;
      // All decodes start at once — `decode` queues units on the decoder in wire
      // order — and each is drawn in wire order as it lands, so a picture is released
      // the moment it is drawn instead of the whole batch's worth staying alive until
      // the slowest.
      const decodes = records.map(decode);
      for (let i = 0; i < records.length; i += 1) {
        const record = records[i];
        if (record.kind === "graphics") {
          await compose(record, born);
          continue;
        }
        const image = await decodes[i];
        if (!image) {
          continue;
        }
        if (generation !== born) {
          // `clear()` ran while this decode was in flight: the previous desktop must
          // not show through on the next attachment's canvas.
          image.close();
          continue;
        }
        try {
          paint(record, image);
        } finally {
          // Whatever became of it: the software decoder waits on this.
          image.close();
        }
      }
    },
    clear() {
      generation += 1;
      releaseVideo();
      // The next attachment names its own part, ahead of any run.
      graphicsPart = null;
    },
    startGraphics() {
      releaseVideo();
      const starting: Pipeline = {
        compositor: null,
        picture: null,
        video: null,
        shown: false,
        broken: false,
        ready: Promise.resolve(),
      };
      // The second display's end, reading the picture of whichever pipeline is
      // current when an update goes out. A channel that cannot be opened leaves
      // the tab unpainted, and this page's picture as it is.
      if (!source) {
        try {
          source = createRelaySource(makeRelay(), () =>
            pipeline?.compositor && !pipeline.broken
              ? pipeline.compositor.picture()
              : null,
          );
        } catch (error) {
          console.warn("the second display's channel did not open:", error);
        }
      }
      starting.ready = loadCompositor()
        .then((make) => {
          // Replaced or cleared while the module loaded: nothing to make one for.
          if (!starting.broken) {
            starting.compositor = make();
            starting.picture = makePicture();
            starting.picture.window(graphicsPart);
          }
        })
        .catch((error: unknown) => {
          if (!starting.broken) {
            finish(starting);
            options.onVideoError(
              `This browser could not load the graphics compositor (${describe(error)}).`,
            );
          }
        });
      pipeline = starting;
    },
    setGraphicsView(part) {
      graphicsPart = part;
      const current = pipeline;
      if (!current?.picture || current.broken) {
        return;
      }
      try {
        current.picture.window(part);
      } catch (error) {
        endPipeline(current, describe(error));
      }
    },
    mirrorGraphics(display, part) {
      if (!mirror) {
        // A tab holds no pipeline and no stream: the picture is the whole of
        // what it shows.
        releaseVideo();
        let picture: GraphicsPicture;
        try {
          picture = makePicture();
        } catch (error) {
          options.onVideoError(
            `This browser could not show display ${display}'s picture (${describe(error)}).`,
          );
          return;
        }
        // Shown from its first painted update, and not before: until then the
        // picture's canvas holds nothing of the display's.
        const made: Mirror = {
          picture,
          shown: false,
          sink: createRelaySink(
            makeRelay(),
            picture,
            () => {
              if (mirror === made && !made.shown) {
                made.shown = true;
                options.onGraphicsShown?.(true);
              }
            },
            (why) => {
              if (mirror === made) {
                releaseMirror();
                videoComplained = false;
                options.onVideoError(
                  `This browser could not show display ${display}'s picture (${why}). Reload the page to start it over.`,
                );
              }
            },
          ),
        };
        mirror = made;
      }
      mirror.sink.show(display, part);
    },
    blank(w, h) {
      hidePlanes();
      if (mirror) {
        try {
          mirror.picture.blank(w, h);
        } catch (error) {
          const why = describe(error);
          releaseMirror();
          options.onVideoError(
            `This browser could not show the display's picture (${why}). Reload the page to start it over.`,
          );
        }
        return;
      }
      const current = pipeline;
      if (!current?.picture || current.broken) {
        return;
      }
      try {
        current.picture.blank(w, h);
      } catch (error) {
        endPipeline(current, describe(error));
      }
    },
    setVideoFormat(format) {
      // A stream takes the picture back from a pipeline: a host that draws with
      // bitmap updates after all, which the gateway encodes. In a tab, from the
      // mirror of the session page's picture, which shows the display no more.
      releasePipeline();
      releaseMirror();
      if (refused !== null && format.decode !== refused) {
        // Not the configuration that was refused, so the refusal no longer stands —
        // but the banner stays until a frame paints, as any other complaint's does.
        refused = null;
        videoComplained = true;
      }
      desktopVideo().setFormat(format);
    },
  };
}
