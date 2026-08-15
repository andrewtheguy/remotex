/// <reference types="vite/client" />

/** Compile-time app version, injected from Cargo.toml (see vite.config.ts). */
declare const __APP_VERSION__: string;

/**
 * MediaStreamTrackProcessor (mediacapture-transform), which TypeScript's DOM
 * lib does not ship: the spec is a working draft implemented by Chromium, and
 * Chrome or Edge is this client's platform (see CLAUDE.md). Only what
 * cameraSender.ts touches is declared — the readable side, over video — and
 * the runtime check there is what keeps a browser without it at a named error
 * rather than a crash.
 */
declare class MediaStreamTrackProcessor {
  constructor(init: { track: MediaStreamTrack });
  readonly readable: ReadableStream<VideoFrame>;
}
