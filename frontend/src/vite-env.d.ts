/// <reference types="vite/client" />

/** Compile-time app version, injected from Cargo.toml (see vite.config.ts). */
declare const __APP_VERSION__: string;

/**
 * MediaStreamTrackProcessor (mediacapture-transform), which TypeScript's DOM
 * lib does not ship: the spec is a working draft implemented by Chromium, and
 * Chrome or Edge is this client's platform (see CLAUDE.md). Only what
 * cameraSender.ts and micSender.ts touch is declared — the readable side, over
 * video or audio — and the runtime checks there are what keep a browser without
 * it at a named error rather than a crash.
 */
/**
 * Opus's mode and signal hints (WebCodecs Opus codec registration), which
 * Chromium's AudioEncoder takes and TypeScript's DOM lib does not yet name.
 * micSender.ts asks for voice.
 */
interface OpusEncoderConfig {
  application?: "voip" | "audio" | "lowdelay";
  signal?: "auto" | "music" | "voice";
}

declare class MediaStreamTrackProcessor<T = VideoFrame> {
  constructor(init: { track: MediaStreamTrack });
  readonly readable: ReadableStream<T>;
}
