// The downstream half of the bridge: one held-open response the app writes
// envelopes into.
//
// A `fetch` rather than a WebSocket because this direction is all that needs
// bytes, and the app's listener stays a plain HTTP server (see
// Sources/Canvas/CanvasServer.swift). Upstream is the script message handler,
// which is already ordered and carries nothing bigger than a mouse position.

import { createEnvelopeReader, type Envelope } from "./envelope.ts";

/** Backoff between attempts, in milliseconds. */
const RETRY_MS = 250;
const MAX_RETRY_MS = 4000;

export interface StreamHandlers {
  /** One assembled message. Thrown errors are caught and reconnect the stream. */
  onEnvelope: (envelope: Envelope) => void;
  /**
   * A stream became live. Every attachment starts from nothing — the app answers
   * this by re-sending the current size and asking the gateway to repaint — so a
   * reconnect needs no state carried across it.
   */
  onOpen: () => void;
}

/**
 * Read `url` forever, reconnecting if it ends.
 *
 * The app closes this stream when it goes away, and there is nothing else to
 * connect to, so retrying is not optimism: it is what makes a reloaded page (or
 * a Web Inspector reload) come back on its own.
 */
export function readStream(url: string, handlers: StreamHandlers): void {
  let delay = RETRY_MS;

  const attempt = async () => {
    const reader = createEnvelopeReader();
    const response = await fetch(url, { cache: "no-store" });
    if (!response.ok || !response.body) {
      throw new Error(`stream answered ${response.status}`);
    }
    handlers.onOpen();
    delay = RETRY_MS;
    const body = response.body.getReader();
    for (;;) {
      const { done, value } = await body.read();
      if (done) {
        return;
      }
      for (const envelope of reader.push(value)) {
        handlers.onEnvelope(envelope);
      }
    }
  };

  const loop = () => {
    attempt()
      .catch((cause) => {
        console.warn("bridge stream ended", cause);
      })
      .finally(() => {
        setTimeout(loop, delay);
        delay = Math.min(delay * 2, MAX_RETRY_MS);
      });
  };
  loop();
}
