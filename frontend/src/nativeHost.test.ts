// The shape the page hands the shell, which is the one interface neither side's
// type checker is in a position to police: the bridge is exposed at runtime by the
// app's preload, so this module is writing to something that exists in this repo
// only as a `type` it could keep in step with itself while drifting from the thing
// it describes.
//
// No browser test can stand in for it: in a browser `NATIVE_HOST` is false and
// every export here returns without doing anything, which is indistinguishable from
// a bridge that is being called and dropping everything.
import assert from "node:assert/strict";
import { test } from "node:test";

type Handler = (command: unknown) => void;

// Installed before the module is imported, because `NATIVE_HOST` is read once at
// module load — in the app the preload has run before any script in the document.
const posted: unknown[] = [];
const handlers = new Set<Handler>();
const fake = globalThis as unknown as {
  window: unknown;
  remotexNative: {
    post: (event: unknown) => void;
    onCommand: (handler: Handler) => () => void;
  };
};
fake.window = globalThis;
fake.remotexNative = {
  post: (event: unknown) => {
    posted.push(event);
  },
  onCommand: (handler: Handler) => {
    handlers.add(handler);
    return () => handlers.delete(handler);
  },
};

const { NATIVE_HOST, postToHost } = await import("./nativeHost.ts");

test("a page inside the app knows it", () => {
  assert.equal(NATIVE_HOST, true);
});

test("an event crosses as the object, not as a string", () => {
  posted.length = 0;
  postToHost({ type: "clipboardFromRemote", text: "hello" });

  assert.equal(posted.length, 1);
  // Structured, because the shell's IPC clones it: re-encoding here would mean two
  // descriptions of the same payload, and only one of them type-checked.
  assert.deepEqual(posted[0], { type: "clipboardFromRemote", text: "hello" });
});

test("a bridge that throws does not take the page down with it", () => {
  const good = fake.remotexNative.post;
  fake.remotexNative.post = () => {
    throw new Error("the app is gone");
  };
  assert.doesNotThrow(() => postToHost({ type: "unauthenticated" }));
  fake.remotexNative.post = good;
});

// What is deliberately *not* tested here: that `useNativeCommands` detaches on
// unmount. The subscription is the return value of a `useEffect`, so the only thing
// that can exercise it is React — and a test that calls the fake's `onCommand` and
// then its disposer, as one here used to, asserts that a Set in this file adds and
// deletes. That reads as coverage of the contract and is coverage of nothing.
