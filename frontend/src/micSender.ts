// The microphone sender: this browser's mic, captured as PCM and fed to
// `/ws/mic`, whose far end is a virtual microphone plugged into the remote.
//
// The camera sender's twin, one direction over. Opening the socket is the enable
// and closing it is the disable — the same contract as the camera's, with the
// media going the same way (browser to remote). The gateway never transcodes:
// raw signed-16-bit PCM is what leaves here and what MS-RDPEAI carries, the same
// bargain `audio_codec = "pcm"` strikes for the remote's own sound.
//
// Unlike the camera this sender is purely reactive: it announces no format,
// because MS-RDPEAI lets the *host* choose the sample rate and channel count.
// It waits for the remote's `micOpen`, builds a capture graph at exactly that
// rate and channel count — an `AudioContext` resamples the mic to it, so there
// is no encoder and no manual resampler here — and streams until `micClose` or
// the socket closes.

import type { ControlMsg } from "./protocol";

export interface MicSenderCallbacks {
  // The socket closed or the sender failed, and the sender has already stopped:
  // the capture is released and the mic is off. `reason` is non-null for a
  // failure worth showing (no mic permission, no audio capture, the target
  // refusing the socket) and null for an ordinary close.
  onStopped: (reason: string | null) => void;
  // The remote started or stopped consuming — an application over there opened
  // or closed the microphone. UI feedback only; the sender already obeys.
  onStreaming: (streaming: boolean) => void;
}

export interface MicSender {
  stop: () => void;
}

// Bytes allowed to sit unsent in the socket before buffers are dropped instead
// of queued. `send` queues without limit, so a slow uplink would otherwise
// become unbounded memory and a microphone ever further behind the speaker.
// What this drops is whole PCM buffers, which — unlike H.264 — cost nothing
// beyond themselves: audio has no GOP, so the next buffer decodes on its own.
// A quarter second at 48 kHz stereo 16-bit is about 96 KB; a quarter megabyte
// is comfortable headroom above one capture chunk.
const MAX_BUFFERED_BYTES = 256 * 1024;

// One capture chunk, in seconds: how much PCM the worklet accumulates before
// posting it. 20 ms is the usual real-time audio packet — small enough for low
// latency, large enough that the message rate is a few dozen a second rather
// than the ~375/s a bare 128-frame render quantum would cost.
const CHUNK_SECONDS = 0.02;

// The AudioWorklet processor, as a module string so there is no separate file
// for the bundler to place and no second build entry. It downmixes/upmixes to
// the requested channel count (the node's `channelCount` does the mixing), packs
// signed-16-bit interleaved PCM, and posts whole chunks as transferable buffers.
const WORKLET_MODULE = `
class MicCaptureProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const opts = options.processorOptions;
    this.channels = opts.channels;
    this.chunkFrames = opts.chunkFrames;
    this.buffer = new Int16Array(this.chunkFrames * this.channels);
    this.filled = 0;
  }
  process(inputs) {
    const input = inputs[0];
    if (!input || input.length === 0) {
      return true;
    }
    const frames = input[0].length;
    for (let i = 0; i < frames; i += 1) {
      for (let c = 0; c < this.channels; c += 1) {
        const channel = input[c] || input[0];
        let sample = channel[i];
        sample = sample < -1 ? -1 : sample > 1 ? 1 : sample;
        this.buffer[this.filled * this.channels + c] =
          sample < 0 ? sample * 0x8000 : sample * 0x7fff;
      }
      this.filled += 1;
      if (this.filled === this.chunkFrames) {
        const chunk = this.buffer.slice(0, this.filled * this.channels);
        this.port.postMessage(chunk.buffer, [chunk.buffer]);
        this.filled = 0;
      }
    }
    return true;
  }
}
registerProcessor("mic-capture", MicCaptureProcessor);
`;

// One live capture graph, torn down as a unit on micClose, a format change, or
// stop.
interface CaptureGraph {
  context: AudioContext;
  source: MediaStreamAudioSourceNode;
  worklet: AudioWorkletNode;
  sink: GainNode;
}

// Capture, connect, and wait for the remote. The returned sender is live until
// its socket closes (server side) or `stop` is called (this side); both end in
// exactly one `onStopped`.
//
// Must be called from a user gesture — `getUserMedia`'s permission prompt is the
// microphone counterpart of the camera's and the AudioContext rule on the audio
// path.
export async function startMicSender(
  url: string,
  callbacks: MicSenderCallbacks,
): Promise<MicSender> {
  // Named refusals before anything is opened, the camera sender's rule: a
  // browser missing one capability costs one feature, not the page.
  if (typeof AudioContext === "undefined") {
    throw new Error("this browser has no AudioContext");
  }
  if (!("audioWorklet" in AudioContext.prototype)) {
    throw new Error("this browser has no AudioWorklet");
  }
  if (!navigator.mediaDevices?.getUserMedia) {
    throw new Error("this browser offers no microphone capture");
  }

  const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  const track = stream.getAudioTracks()[0];
  if (!track) {
    for (const t of stream.getTracks()) {
      t.stop();
    }
    throw new Error("the microphone produced no audio track");
  }

  let stopped = false;
  let graph: CaptureGraph | null = null;
  // Bumped on every open/close/stop so an async graph build (the worklet module
  // loads asynchronously) can tell whether it is still the one wanted — the same
  // generation guard the camera sender uses around its encoder.
  let generation = 0;

  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";

  const tearDownGraph = () => {
    if (!graph) {
      return;
    }
    const { context, source, worklet, sink } = graph;
    graph = null;
    try {
      source.disconnect();
      worklet.disconnect();
      sink.disconnect();
      worklet.port.onmessage = null;
    } catch {
      // A node already gone with its context: nothing left to disconnect.
    }
    void context.close().catch(() => {});
  };

  // One stop for every path out, idempotent: the socket's close handler and an
  // explicit `stop` can both fire, and the second must find nothing to do.
  const stop = (reason: string | null = null) => {
    if (stopped) {
      return;
    }
    stopped = true;
    generation += 1;
    tearDownGraph();
    for (const t of stream.getTracks()) {
      t.stop();
    }
    if (
      socket.readyState === WebSocket.OPEN ||
      socket.readyState === WebSocket.CONNECTING
    ) {
      socket.close();
    }
    callbacks.onStopped(reason);
  };

  // Build a capture graph at the host's chosen rate and channel count. The
  // AudioContext resamples the mic to `sampleRate` for us, and the node's
  // explicit `channelCount` mixes the mic to `channels`, so the worklet only has
  // to pack what it is handed. Guarded by `generation`: a micClose or a newer
  // micOpen arriving while the worklet module loads discards this build.
  const openGraph = async (sampleRate: number, channels: number) => {
    tearDownGraph();
    generation += 1;
    const mine = generation;
    // Constructing the context can throw synchronously when the host's rate is
    // one this browser will not open; there is nothing to close yet if it does.
    let context: AudioContext;
    try {
      context = new AudioContext({ sampleRate });
    } catch (e) {
      stop(
        e instanceof Error
          ? e.message
          : "this browser refused the audio format",
      );
      return;
    }
    const moduleUrl = URL.createObjectURL(
      new Blob([WORKLET_MODULE], { type: "application/javascript" }),
    );
    try {
      await context.audioWorklet.addModule(moduleUrl);
    } catch (e) {
      void context.close().catch(() => {});
      stop(e instanceof Error ? e.message : "the audio worklet failed to load");
      return;
    } finally {
      URL.revokeObjectURL(moduleUrl);
    }
    // Superseded while the module loaded: drop this graph silently.
    if (stopped || mine !== generation) {
      void context.close().catch(() => {});
      return;
    }
    const source = context.createMediaStreamSource(stream);
    const chunkFrames = Math.max(1, Math.round(sampleRate * CHUNK_SECONDS));
    const worklet = new AudioWorkletNode(context, "mic-capture", {
      numberOfInputs: 1,
      numberOfOutputs: 1,
      outputChannelCount: [channels],
      channelCount: channels,
      channelCountMode: "explicit",
      channelInterpretation: "speakers",
      processorOptions: { channels, chunkFrames },
    });
    worklet.port.onmessage = (ev: MessageEvent<ArrayBuffer>) => {
      if (stopped || socket.readyState !== WebSocket.OPEN) {
        return;
      }
      // Backpressure: a microphone buffer is worthless late, and audio has no
      // GOP, so a backed-up uplink drops whole buffers rather than queueing them.
      if (socket.bufferedAmount > MAX_BUFFERED_BYTES) {
        return;
      }
      socket.send(ev.data);
    };
    // A zero-gain sink to the destination keeps the graph pulling the worklet
    // without playing the mic back through this browser's own speakers.
    const sink = context.createGain();
    sink.gain.value = 0;
    source.connect(worklet);
    worklet.connect(sink);
    sink.connect(context.destination);
    void context.resume().catch(() => {});
    graph = { context, source, worklet, sink };
    callbacks.onStreaming(true);
  };

  const closeGraph = () => {
    generation += 1;
    tearDownGraph();
    callbacks.onStreaming(false);
  };

  socket.onmessage = (ev) => {
    if (typeof ev.data !== "string") {
      return;
    }
    let msg: ControlMsg;
    try {
      msg = JSON.parse(ev.data) as ControlMsg;
    } catch {
      return;
    }
    switch (msg.type) {
      case "micOpen":
        void openGraph(msg.sampleRate, msg.channels);
        break;
      case "micClose":
        closeGraph();
        break;
    }
  };

  socket.onclose = (ev) => {
    // 4002 is the gateway saying the target carries no microphone (or the engine
    // is gone); everything else is an ordinary end of the enable.
    stop(ev.code === 4002 ? "this target carries no microphone" : null);
  };

  return { stop: () => stop(null) };
}
