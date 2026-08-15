// The camera sender: this browser's webcam, encoded to H.264 and fed to
// `/ws/camera`, whose far end is a virtual camera plugged into the remote.
//
// Opening the socket is the enable and closing it is the disable — the same
// contract as audio's socket, with the media going the other way. The gateway
// never transcodes: what `VideoEncoder` emits here is what the remote decodes,
// which is why the encoder is configured for Annex B H.264 (the bitstream
// MS-RDPECAM carries) and why a browser that cannot encode it gets a named
// error instead of a fallback. The remote drives the traffic: frames are
// encoded and sent only between its `cameraStart` and `cameraStop`, and a
// `cameraKeyframe` makes the next frame an IDR so a stream the gateway had to
// drop can resume.

import { type ClientMsg, encodeCameraFrame } from "./protocol";

export interface CameraSenderCallbacks {
  // The socket closed or the sender failed, and the sender has already stopped:
  // the capture is released and the camera light is off. `reason` is non-null
  // for a failure worth showing (no camera permission, no H.264 encoder, the
  // target refusing the socket) and null for an ordinary close.
  onStopped: (reason: string | null) => void;
  // The remote started or stopped consuming — an application over there opened
  // or closed the camera. UI feedback only; the sender already obeys.
  onStreaming: (streaming: boolean) => void;
}

export interface CameraSender {
  stop: () => void;
}

// Frames allowed inside the encoder before new ones are skipped instead of
// queued. Skipping *input* is safe anywhere — an unencoded frame constrains no
// GOP — so this is free latency protection against an encoder slower than the
// camera.
const MAX_ENCODE_QUEUE = 2;

// Bytes allowed to sit unsent in the socket before samples are dropped instead
// of queued: `send` itself queues without limit, so a slow uplink would
// otherwise become unbounded memory and a picture ever further behind the
// camera. Unlike the encoder queue above, what this drops is *output*, which
// is never free in H.264 — everything until the next keyframe goes with the
// first dropped delta, and that keyframe is asked for once the backlog clears,
// the same drop-whole-and-rekey the gateway's own credit queue applies. A
// quarter of a megabyte is a quarter second at the ceiling bitrate above.
const MAX_BUFFERED_BYTES = 256 * 1024;

// The H.264 configuration for one capture geometry. Split out pure so a unit
// test can pin the level and bitrate choices without a camera.
//
// Constrained Baseline, because the far decoder is unknowable from here — it is
// whatever camera stack the remote's application brings — and Constrained
// Baseline is the profile everything decodes. The level is the smallest of
// 3.1/4.0/5.0 that fits the geometry's macroblock rate, which is how the level
// byte stays honest for a 4K camera without a table of every level. Bitrate at
// 0.1 bits per pixel per frame — the usual realtime-video rule of thumb —
// clamped to a floor a tiny capture still looks fine at and a ceiling a 4K one
// cannot flood the uplink with.
export function h264Config(
  width: number,
  height: number,
  fps: number,
): { codec: string; bitrate: number } {
  const macroblocks = Math.ceil(width / 16) * Math.ceil(height / 16);
  const mbRate = macroblocks * fps;
  // Level limits from the H.264 spec's table A-1 (macroblocks per second).
  const level = mbRate <= 108_000 ? "1f" : mbRate <= 245_760 ? "28" : "32";
  const bitrate = Math.min(
    8_000_000,
    Math.max(300_000, width * height * fps * 0.1),
  );
  return { codec: `avc1.42e0${level}`, bitrate: Math.round(bitrate) };
}

// Reduce the browser's fractional frame rate to the rational both wire formats
// carry (29.97 → 30000/1001 territory; integers stay {fps, 1}).
export function rationalFps(fps: number): {
  numerator: number;
  denominator: number;
} {
  const numerator = Math.round(fps * 1000);
  const gcd = (a: number, b: number): number => (b === 0 ? a : gcd(b, a % b));
  const divisor = gcd(numerator, 1000);
  return { numerator: numerator / divisor, denominator: 1000 / divisor };
}

// Capture, configure, connect. The returned sender is live until its socket
// closes (server side) or `stop` is called (this side); both end in exactly one
// `onStopped`.
//
// Must be called from a user gesture — `getUserMedia`'s permission prompt is
// the camera counterpart of the AudioContext rule on the audio path.
export async function startCameraSender(
  url: string,
  callbacks: CameraSenderCallbacks,
): Promise<CameraSender> {
  // Named refusals before anything is opened. WebCodecs is the client's entry
  // condition (preflight.ts), but that gate checks the *decoders* this page
  // cannot run without; the encoder and the track processor are checked here,
  // where a browser without them costs one feature instead of the page.
  if (typeof VideoEncoder === "undefined") {
    throw new Error("this browser has no VideoEncoder");
  }
  if (typeof MediaStreamTrackProcessor === "undefined") {
    throw new Error(
      "this browser cannot read camera frames (no MediaStreamTrackProcessor)",
    );
  }
  if (!navigator.mediaDevices?.getUserMedia) {
    throw new Error("this browser offers no camera capture");
  }

  const stream = await navigator.mediaDevices.getUserMedia({ video: true });
  const track = stream.getVideoTracks()[0];
  if (!track) {
    for (const t of stream.getTracks()) {
      t.stop();
    }
    throw new Error("the camera produced no video track");
  }

  const settings = track.getSettings();
  const width = settings.width ?? 640;
  const height = settings.height ?? 480;
  const fps =
    settings.frameRate && settings.frameRate > 0 ? settings.frameRate : 30;
  const { numerator, denominator } = rationalFps(fps);
  const { codec, bitrate } = h264Config(width, height, fps);

  const config: VideoEncoderConfig = {
    codec,
    width,
    height,
    bitrate,
    framerate: fps,
    latencyMode: "realtime",
    // Annex B, because that is the H.264 MS-RDPECAM carries: parameter sets
    // travel inside each keyframe, so the wire needs no side channel for them.
    avc: { format: "annexb" },
  };
  const support = await VideoEncoder.isConfigSupported(config);
  if (!support.supported) {
    track.stop();
    throw new Error(
      `this browser cannot encode ${codec} at ${width}x${height}`,
    );
  }

  let stopped = false;
  let streaming = false;
  let forceKeyframe = true;
  // Whether the socket backed up past MAX_BUFFERED_BYTES and deltas are being
  // dropped. Set by the encoder's output, cleared by the keyframe that resumes
  // the stream; the pump requests that keyframe when the backlog has cleared.
  let droppingDeltas = false;

  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";

  const encoder = new VideoEncoder({
    output: (chunk) => {
      if (stopped || socket.readyState !== WebSocket.OPEN) {
        return;
      }
      const key = chunk.type === "key";
      // Backpressure. A delta after a dropped delta is undecodable, so the
      // first drop commits to dropping every delta until the next keyframe.
      // Keyframes always pass: one is what lets the stream resume.
      if (
        !key &&
        (droppingDeltas || socket.bufferedAmount > MAX_BUFFERED_BYTES)
      ) {
        droppingDeltas = true;
        return;
      }
      if (key) {
        droppingDeltas = false;
      }
      const unit = new Uint8Array(chunk.byteLength);
      chunk.copyTo(unit);
      const frame = encodeCameraFrame(unit, key);
      if (frame) {
        socket.send(frame);
      }
    },
    error: (e) => stop(e.message || "the H.264 encoder failed"),
  });
  encoder.configure(config);

  const processor = new MediaStreamTrackProcessor({ track });
  const reader = processor.readable.getReader();

  // One stop for every path out, idempotent: the socket's close handler and an
  // explicit `stop` can both fire, and the second must find nothing to do.
  const stop = (reason: string | null = null) => {
    if (stopped) {
      return;
    }
    stopped = true;
    void reader.cancel().catch(() => {});
    track.stop();
    if (encoder.state !== "closed") {
      encoder.close();
    }
    if (
      socket.readyState === WebSocket.OPEN ||
      socket.readyState === WebSocket.CONNECTING
    ) {
      socket.close();
    }
    callbacks.onStopped(reason);
  };

  socket.onopen = () => {
    const format: ClientMsg = {
      type: "cameraFormat",
      width,
      height,
      fpsNumerator: numerator,
      fpsDenominator: denominator,
    };
    socket.send(JSON.stringify(format));
  };

  socket.onmessage = (ev) => {
    if (typeof ev.data !== "string") {
      return;
    }
    let msg: { type?: string };
    try {
      msg = JSON.parse(ev.data) as { type?: string };
    } catch {
      return;
    }
    switch (msg.type) {
      case "cameraStart":
        streaming = true;
        // Every (re)start opens on an IDR: the remote's decoder starts from
        // parameter sets, and Annex B keyframes carry them.
        forceKeyframe = true;
        callbacks.onStreaming(true);
        break;
      case "cameraStop":
        streaming = false;
        callbacks.onStreaming(false);
        break;
      case "cameraKeyframe":
        forceKeyframe = true;
        break;
    }
  };

  socket.onclose = (ev) => {
    // 4002 is the gateway saying the target carries no camera (or the engine
    // is gone); everything else is an ordinary end of the enable.
    stop(ev.code === 4002 ? "this target carries no camera" : null);
  };

  // One frame's fate: skipped while the remote is not consuming or the encoder
  // is behind — an unencoded frame constrains no GOP — and otherwise encoded,
  // at an IDR when one is owed. The caller closes the frame either way.
  const encodeFrame = (frame: VideoFrame) => {
    if (!streaming || encoder.encodeQueueSize > MAX_ENCODE_QUEUE) {
      return;
    }
    if (droppingDeltas && socket.bufferedAmount <= MAX_BUFFERED_BYTES) {
      // The backlog cleared: resume at the IDR the far decoder needs.
      forceKeyframe = true;
    }
    encoder.encode(frame, { keyFrame: forceKeyframe });
    forceKeyframe = false;
  };

  // The frame pump. Frames flow whenever the camera does; which of them cost
  // anything is the remote's decision — outside start/stop they are closed
  // unencoded, which keeps the capture pipeline drained without spending CPU.
  void (async () => {
    for (;;) {
      let result: ReadableStreamReadResult<VideoFrame>;
      try {
        result = await reader.read();
      } catch {
        break; // cancelled by stop()
      }
      if (result.done || stopped) {
        result.value?.close();
        break;
      }
      encodeFrame(result.value);
      result.value.close();
    }
  })();

  return { stop: () => stop(null) };
}
