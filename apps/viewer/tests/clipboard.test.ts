// The echo guards, which are the whole of why this is not two lines.
//
// Over an injected pasteboard, so nothing here touches the user's own — and no test
// waits out a poll interval: `poll()` is called where the timer would have called it.

import { beforeEach, describe, expect, test } from "bun:test";
import { ClipboardSynchronizer, MAXIMUM_BYTES } from "../src/main/clipboard.ts";

let board = "";
let sent: string[] = [];

function sync(): ClipboardSynchronizer {
  return new ClipboardSynchronizer({
    pasteboard: {
      read: () => board,
      write: (text) => {
        board = text;
      },
    },
    send: (text) => sent.push(text),
  });
}

beforeEach(() => {
  board = "";
  sent = [];
});

describe("following the pasteboard", () => {
  test("enabling sends what is already there", () => {
    // Whatever is on the pasteboard when a desktop comes up is something the user
    // may well be about to paste, and it predates anything this could have seen.
    board = "already copied";
    const clipboard = sync();
    clipboard.update(true);
    expect(sent).toEqual(["already copied"]);
  });

  test("a change is sent once", () => {
    const clipboard = sync();
    clipboard.update(true);
    board = "new";
    clipboard.poll();
    clipboard.poll();
    expect(sent).toEqual(["new"]);
  });

  test("nothing is sent while disabled", () => {
    const clipboard = sync();
    board = "copied off the desktop";
    clipboard.poll();
    expect(sent).toEqual([]);
  });

  test("an oversized value is skipped, not truncated", () => {
    // The gateway refuses it in either direction; the remote keeps what it had,
    // which is better than being handed half of something.
    const clipboard = sync();
    clipboard.update(true);
    board = "x".repeat(MAXIMUM_BYTES + 1);
    clipboard.poll();
    expect(sent).toEqual([]);
  });
});

describe("the echo guards", () => {
  test("what came from the remote is not sent back to it", () => {
    const clipboard = sync();
    clipboard.update(true);
    clipboard.receiveFromRemote("from the guest");
    expect(board).toBe("from the guest");
    // The write moved the pasteboard, and without the guard the next poll would
    // hand the guest its own text back as though the user had copied it here.
    clipboard.poll();
    expect(sent).toEqual([]);
  });

  test("what was sent to the remote is not written back over the local one", () => {
    const clipboard = sync();
    clipboard.update(true);
    board = "typed here";
    clipboard.poll();
    expect(sent).toEqual(["typed here"]);
    clipboard.receiveFromRemote("typed here");
    expect(sent).toEqual(["typed here"]);
  });

  test("the same remote value twice is written once", () => {
    const clipboard = sync();
    clipboard.update(true);
    clipboard.receiveFromRemote("same");
    board = "something else";
    clipboard.receiveFromRemote("same");
    // The second push is dropped as already mirrored, so a local value that arrived
    // in between is not quietly overwritten.
    expect(board).toBe("something else");
  });

  test("a newer local value wins over an older remote one", () => {
    const clipboard = sync();
    clipboard.update(true);
    board = "just copied here";
    clipboard.receiveFromRemote("stale from the guest");
    expect(board).toBe("just copied here");
    expect(sent).toEqual(["just copied here"]);
  });

  test("an empty clipboard is not an update", () => {
    const clipboard = sync();
    clipboard.update(true);
    clipboard.receiveFromRemote("");
    expect(board).toBe("");
    expect(sent).toEqual([]);
  });
});

describe("stopping", () => {
  test("disabling forgets what was mirrored", () => {
    // The next desktop is a different session; a guard held over from the last one
    // would silently drop its first clipboard.
    const clipboard = sync();
    clipboard.update(true);
    clipboard.receiveFromRemote("from the guest");
    clipboard.update(false);
    clipboard.update(true);
    expect(sent).toEqual(["from the guest"]);
  });

  test("the timer is cancelled with it", () => {
    let cancelled = false;
    const clipboard = new ClipboardSynchronizer({
      pasteboard: { read: () => board, write: () => {} },
      send: () => {},
      schedule: () => () => {
        cancelled = true;
      },
    });
    clipboard.update(true);
    clipboard.update(false);
    expect(cancelled).toBe(true);
  });
});
