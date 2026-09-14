// The microphone sender: this browser's microphone, encoded as speech-grade
// Opus and fed to `/ws/mic`, whose far end is a recording device on the remote.
//
// Opening the socket is the enable and closing it is the disable, the camera
// sender's contract. What differs is what the audio is for. The remote's own
// sound and picture aim to be the desktop as it would be in front of you, and
// spend the bandwidth that takes; a redirected microphone is somebody talking,
// wanted now and then, so it is sent as cheaply as speech can be — mono Opus in
// voice mode at a low bitrate, on any link. The gateway decodes it to the PCM
// the remote records in. Nothing is encoded until an application over there
// starts recording (`micOpen`), and nothing after it stops (`micClose`).

import { encodeMicFrame } from "./protocol";

export interface MicSenderCallbacks {
  // The socket closed or the sender failed, and the sender has already
  // stopped: the capture is released. `reason` is non-null for a failure worth
  // showing (no permission, no Opus encoder, the target refusing the socket)
  // and null for an ordinary close.
  onStopped: (reason: string | null) => void;
  // The remote started or stopped recording. UI feedback only.
  onStreaming: (streaming: boolean) => void;
}

export interface MicSender {
  stop: () => void;
}

// Speech, and no more than speech needs: wideband voice at 16 kbit/s.
export const MIC_BITRATE = 16_000;
// 60 ms packets. Every packet costs a WebSocket frame's header on top of the
// Opus, which at 20 ms would be a fifth of this bitrate again; a microphone
// can spare the extra 40 ms.
export const MIC_FRAME_MICROSECONDS = 60_000;

// Bytes allowed unsent before packets are dropped instead of queued: a
// microphone behind the speaker is worse than one missing a moment, and an
// Opus decoder takes a lost packet in its stride. About eight seconds of this
// bitrate — only a link that has stopped reaches it.
const MAX_BUFFERED_BYTES = 16 * 1024;

// Captured buffers allowed waiting in the encoder before new ones are dropped.
const MAX_ENCODE_QUEUE = 4;

// The encoder configuration for audio captured at `sampleRate`: always mono,
// in Opus's voice mode.
export function opusConfig(sampleRate: number): AudioEncoderConfig {
  return {
    codec: "opus",
    sampleRate,
    numberOfChannels: 1,
    bitrate: MIC_BITRATE,
    bitrateMode: "variable",
    opus: {
      application: "voip",
      signal: "voice",
      frameDuration: MIC_FRAME_MICROSECONDS,
    },
  };
}

// Average planar channels into one.
export function downmix(planes: Float32Array[]): Float32Array<ArrayBuffer> {
  const frames = planes[0]?.length ?? 0;
  const mono = new Float32Array(frames);
  for (const plane of planes) {
    for (let i = 0; i < frames; i += 1) {
      mono[i] += plane[i] / planes.length;
    }
  }
  return mono;
}

// The mono AudioData to encode: the captured one when it already is, and a
// downmix of it otherwise. The caller closes what it passed in.
function monoOf(data: AudioData): AudioData {
  if (data.numberOfChannels === 1) {
    return data;
  }
  const planes: Float32Array[] = [];
  for (let plane = 0; plane < data.numberOfChannels; plane += 1) {
    const samples = new Float32Array(data.numberOfFrames);
    data.copyTo(samples, { planeIndex: plane, format: "f32-planar" });
    planes.push(samples);
  }
  return new AudioData({
    format: "f32-planar",
    sampleRate: data.sampleRate,
    numberOfFrames: data.numberOfFrames,
    numberOfChannels: 1,
    timestamp: data.timestamp,
    data: downmix(planes),
  });
}

// Capture, connect, and wait for the remote. The returned sender is live
// until its socket closes (server side) or `stop` is called (this side); both
// end in exactly one `onStopped`.
//
// Must be called from a user gesture — `getUserMedia`'s permission prompt.
export async function startMicSender(
  url: string,
  callbacks: MicSenderCallbacks,
): Promise<MicSender> {
  if (typeof AudioEncoder === "undefined") {
    throw new Error("this browser has no AudioEncoder");
  }
  if (typeof MediaStreamTrackProcessor === "undefined") {
    throw new Error(
      "this browser cannot read microphone audio (no MediaStreamTrackProcessor)",
    );
  }
  if (!navigator.mediaDevices?.getUserMedia) {
    throw new Error("this browser offers no microphone capture");
  }

  // The browser's own speech processing, all of it: a remote call hears this
  // microphone through the remote's speakers and this browser's, so echo
  // cancellation matters as much as on any call.
  const stream = await navigator.mediaDevices.getUserMedia({
    audio: {
      channelCount: { ideal: 1 },
      echoCancellation: true,
      noiseSuppression: true,
      autoGainControl: true,
    },
  });
  const release = () => {
    for (const t of stream.getTracks()) {
      t.stop();
    }
  };
  const track = stream.getAudioTracks()[0];
  if (!track) {
    release();
    throw new Error("the microphone produced no audio track");
  }
  const rate = track.getSettings().sampleRate ?? 48_000;
  const support = await AudioEncoder.isConfigSupported(opusConfig(rate)).catch(
    () => ({ supported: false }),
  );
  if (!support.supported) {
    release();
    throw new Error("this browser cannot encode Opus from the microphone");
  }

  let stopped = false;
  let streaming = false;
  // The rate the encoder is configured for, which follows the audio.
  let configuredRate = 0;

  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";

  const encoder = new AudioEncoder({
    output: (chunk) => {
      if (
        stopped ||
        !streaming ||
        socket.readyState !== WebSocket.OPEN ||
        socket.bufferedAmount > MAX_BUFFERED_BYTES
      ) {
        return;
      }
      const packet = new Uint8Array(chunk.byteLength);
      chunk.copyTo(packet);
      const frame = encodeMicFrame(packet);
      if (frame) {
        socket.send(frame);
      }
    },
    error: (e) => stop(e.message || "the Opus encoder failed"),
  });

  const processor = new MediaStreamTrackProcessor<AudioData>({ track });
  const reader = processor.readable.getReader();

  const stop = (reason: string | null = null) => {
    if (stopped) {
      return;
    }
    stopped = true;
    void reader.cancel().catch(() => {});
    release();
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
    if (msg.type === "micOpen" || msg.type === "micClose") {
      streaming = msg.type === "micOpen";
      // Packets still in the encoder belong to the recording that ended.
      if (!streaming && encoder.state === "configured") {
        encoder.reset();
        configuredRate = 0;
      }
      callbacks.onStreaming(streaming);
    }
  };

  socket.onclose = (ev) => {
    // 4002 is the gateway saying the target carries no microphone.
    stop(ev.code === 4002 ? "this target carries no microphone" : null);
  };

  const encode = (data: AudioData) => {
    if (!streaming || encoder.state === "closed") {
      return;
    }
    if (data.sampleRate !== configuredRate) {
      if (encoder.state === "configured") {
        encoder.reset();
      }
      encoder.configure(opusConfig(data.sampleRate));
      configuredRate = data.sampleRate;
    }
    // An encoder falling behind the microphone drops audio rather than lagging it.
    if (encoder.encodeQueueSize > MAX_ENCODE_QUEUE) {
      return;
    }
    const mono = monoOf(data);
    encoder.encode(mono);
    if (mono !== data) {
      mono.close();
    }
  };

  // One captured buffer's fate, closed either way.
  const take = (data: AudioData) => {
    try {
      encode(data);
    } catch (e) {
      stop(
        e instanceof Error ? e.message : "the microphone could not be encoded",
      );
    } finally {
      data.close();
    }
  };

  // The capture pump. Audio flows whenever the microphone does; it costs
  // anything only while the remote is recording.
  void (async () => {
    for (;;) {
      let result: ReadableStreamReadResult<AudioData>;
      try {
        result = await reader.read();
      } catch {
        break; // cancelled by stop()
      }
      if (result.done || stopped) {
        result.value?.close();
        break;
      }
      take(result.value);
    }
  })();

  return { stop: () => stop(null) };
}
