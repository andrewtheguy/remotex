import { describe, expect, test } from "bun:test";
import { LogTail, TAIL_LINES } from "../src/main/logtail.ts";

describe("the gateway's stderr", () => {
  test("chunks are not lines", () => {
    const tail = new LogTail();
    tail.push("one li");
    tail.push("ne\ntwo\nthr");
    tail.push("ee\n");
    expect(tail.text()).toBe("one line\ntwo\nthree");
  });

  test("an unfinished last line is kept", () => {
    // A program dying part way through its explanation is exactly when this is
    // read, and that line is the one that matters.
    const tail = new LogTail();
    tail.push("error: could not");
    expect(tail.text()).toBe("error: could not");
    expect(tail.isEmpty()).toBe(false);
  });

  test("only the end is kept", () => {
    const tail = new LogTail();
    for (let i = 0; i < TAIL_LINES * 3; i++) {
      tail.push(`line ${i}\n`);
    }
    const lines = tail.text().split("\n");
    expect(lines).toHaveLength(TAIL_LINES);
    expect(lines[lines.length - 1]).toBe(`line ${TAIL_LINES * 3 - 1}`);
  });

  test("silence is distinguishable from output", () => {
    const tail = new LogTail();
    expect(tail.isEmpty()).toBe(true);
    tail.push("\n");
    expect(tail.isEmpty()).toBe(false);
  });
});
