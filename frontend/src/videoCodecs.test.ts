// The client's half of the codec negotiation, against a stubbed `VideoDecoder`.
//
// What is worth testing here is not whether a browser decodes VP9 — that is browser QA
// — but that this asks the right question about the right strings, keeps the gateway's
// order, and turns every kind of refusal into a shorter list rather than a throw. A
// throw here would take the connect down with it, and a connect that never happens is a
// worse failure than one the gateway refuses by name.

import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";

const globals = globalThis as unknown as {
  VideoDecoder?: unknown;
  fetch: typeof fetch;
  /** Optional because this runtime is not a browser — `gateway.ts` needs one to exist. */
  window?: unknown;
};
const realFetch = globals.fetch;
const hadVideoDecoder = "VideoDecoder" in globals;
const realVideoDecoder = globals.VideoDecoder;
const hadWindow = "window" in globals;
const realWindow = globals.window;

/** Configuration strings `isConfigSupported` was asked about, in order. */
let asked: string[] = [];
/** Strings the stub accepts; anything else is refused. */
let supported = new Set<string>();
/** Strings the stub refuses to parse at all, as a browser does for a malformed one. */
let malformed = new Set<string>();

const OFFERS = [
  { name: "vp9", probe: "vp09.00.31.08" },
  { name: "h264", probe: "avc1.42E01E" },
];

beforeEach(() => {
  asked = [];
  supported = new Set(OFFERS.map((offer) => offer.probe));
  malformed = new Set();
  globals.VideoDecoder = {
    isConfigSupported(config: { codec: string }) {
      asked.push(config.codec);
      if (malformed.has(config.codec)) {
        return Promise.reject(new TypeError("not a codec string"));
      }
      return Promise.resolve({ supported: supported.has(config.codec) });
    },
  };
  // `gateway.ts` derives the gateway's origin from `window.location` at import time, so
  // one has to exist before the modules under test are imported.
  globals.window = {
    location: { origin: "https://gateway.test" },
    isSecureContext: true,
  };
  globals.fetch = (async () => {
    return {
      json: async () => ({
        branding: "remotex",
        protocolVersion: 12,
        videoCodecs: OFFERS,
      }),
    };
  }) as unknown as typeof fetch;
});

afterEach(() => {
  globals.fetch = realFetch;
  if (hadWindow) {
    globals.window = realWindow;
  } else {
    delete globals.window;
  }
  if (hadVideoDecoder) {
    globals.VideoDecoder = realVideoDecoder;
  } else {
    delete globals.VideoDecoder;
  }
});

// The decision, without the memo in front of it — see `probeVideoCodecs`. The memoized
// entry point has one test, at the bottom, and it is the only case in this file that goes
// through it: a module-level cache cannot be reset, so a second case through it would be
// a test of the cache rather than of the probe.

test("a browser that decodes both accepts both, in the gateway's order", async () => {
  const { probeVideoCodecs } = await import("./videoCodecs.ts");
  assert.deepEqual(await probeVideoCodecs(OFFERS), ["vp9", "h264"]);
  assert.deepEqual(
    asked,
    [OFFERS[0].probe, OFFERS[1].probe],
    "the probe asked about something other than the strings the gateway published",
  );
});

test("a browser without VP9 falls back to H.264 alone", async () => {
  const { probeVideoCodecs } = await import("./videoCodecs.ts");
  supported = new Set([OFFERS[1].probe]);
  assert.deepEqual(await probeVideoCodecs(OFFERS), ["h264"]);
});

test("a browser with no video decoder at all accepts nothing", async () => {
  const { probeVideoCodecs } = await import("./videoCodecs.ts");
  delete globals.VideoDecoder;
  assert.deepEqual(await probeVideoCodecs(OFFERS), []);
  assert.deepEqual(asked, [], "a runtime with no decoder was asked anyway");
});

test("an offer this browser cannot even parse loses only itself", async () => {
  // A gateway newer than the client, offering a codec string this browser rejects
  // outright rather than answering `supported: false`. The other offer must survive.
  const { probeVideoCodecs } = await import("./videoCodecs.ts");
  malformed = new Set([OFFERS[0].probe]);
  assert.deepEqual(await probeVideoCodecs(OFFERS), ["h264"]);
});

test("a gateway that offers no codecs is answered with no codecs", async () => {
  // Older than this client, or a build with the field missing. Not a throw: the gateway
  // refuses a video target by name, and every other target still connects.
  const { probeVideoCodecs } = await import("./videoCodecs.ts");
  assert.deepEqual(await probeVideoCodecs([]), []);
  assert.deepEqual(
    asked,
    [],
    "nothing was offered, so nothing should be asked",
  );
});

// The unreachable and older-gateway cases live in gatewayConfig.test.ts: that module
// memoizes one promise per page, so its fallbacks can only be tested where nothing has
// resolved it yet — which means its own file.

test("the memoized entry point hands every caller the same probe", async () => {
  // Identity rather than a fetch count, so this does not depend on running before the
  // cases above: two calls that are the same promise object cannot be two probes.
  const { acceptedVideoCodecs } = await import("./videoCodecs.ts");
  assert.equal(
    acceptedVideoCodecs(),
    acceptedVideoCodecs(),
    "the probe is not memoized, so every connect would pay for it again",
  );
  // Identity only, and no assertion about *what* it resolved to: `gatewayConfig` memoizes
  // across every file the runner loads into one process, so which offers this saw depends
  // on whether another file resolved it first. What the probe decides is the six cases
  // above, through the function that has no cache in front of it.
  assert.ok(Array.isArray(await acceptedVideoCodecs()));
});
