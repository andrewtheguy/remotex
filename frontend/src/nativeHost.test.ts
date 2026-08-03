// What the page hands the app's query function, which is the one shape neither
// side's type checker is in a position to police: the function is installed by
// Chromium at runtime, so `postToHost` is writing to an interface that exists in
// this repo only as a `type` this file could keep in step with itself while
// drifting from the thing it describes.
//
// It did drift, and silently. A query without `onSuccess` and `onFailure` is
// rejected by the router's renderer half — a missing member reads back as a value
// of type `undefined`, which is not a function — and the rejection reaches neither
// the page nor the app. Every menu in `remotex.app` was permanently disabled,
// because the state they derive from was the message being dropped.
//
// No browser test can stand in for this: in a browser `NATIVE_HOST` is false and
// `postToHost` returns without doing anything, which is exactly what the broken
// version did in the app.
import assert from "node:assert/strict";
import { test } from "node:test";

interface Query {
  request: string;
  onSuccess?: unknown;
  onFailure?: unknown;
}

// Installed before the module is imported, because `NATIVE_HOST` is read once at
// module load — in the app the function is on the page before any script runs.
const posted: Query[] = [];
const fake = globalThis as unknown as {
  window: unknown;
  remotexNative: (query: Query) => number;
};
fake.window = globalThis;
fake.remotexNative = (query: Query) => {
  posted.push(query);
  return posted.length;
};

const { NATIVE_HOST, postToHost } = await import("./nativeHost.ts");

test("a page inside the app knows it", () => {
  assert.equal(NATIVE_HOST, true);
});

test("every post carries the three members the router requires", () => {
  posted.length = 0;
  postToHost({ type: "clipboardFromRemote", text: "hello" });

  assert.equal(posted.length, 1);
  const query = posted[0];
  assert.equal(
    query.request,
    JSON.stringify({ type: "clipboardFromRemote", text: "hello" }),
  );
  // The two that were left off. Their bodies are empty and that is fine; what the
  // router tests is that they are functions.
  assert.equal(typeof query.onSuccess, "function");
  assert.equal(typeof query.onFailure, "function");
});

test("the event is the string, not the object", () => {
  posted.length = 0;
  postToHost({ type: "unauthenticated" });
  assert.equal(typeof posted[0].request, "string");
  assert.deepEqual(JSON.parse(posted[0].request), { type: "unauthenticated" });
});
