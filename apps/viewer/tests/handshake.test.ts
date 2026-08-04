// The one line the gateway prints. `tests/embedded_gateway_e2e.rs` pins the writing
// end; this pins the reading end, and between them the two agree without either
// having to trust the other's description.

import { describe, expect, test } from "bun:test";
import { decodeHandshake, MalformedHandshake } from "../src/main/handshake.ts";

describe("the handshake", () => {
  test("a real one names a port and a token", () => {
    const handshake = decodeHandshake('{"port":49213,"token":"abc"}');
    expect(handshake).toEqual({ port: 49213, token: "abc" });
  });

  test("field order and extra fields do not matter", () => {
    expect(
      decodeHandshake('{"token":"t","port":1,"somethingLater":true}'),
    ).toEqual({ port: 1, token: "t" });
  });

  test("port zero is refused", () => {
    // Zero is what the gateway was *asked* to bind, so reading it back means the
    // real port was never resolved — and a request to :0 fails looking like a
    // network problem rather than like this.
    expect(() => decodeHandshake('{"port":0,"token":"t"}')).toThrow(
      MalformedHandshake,
    );
  });

  test("a missing or empty token is refused", () => {
    expect(() => decodeHandshake('{"port":1}')).toThrow(MalformedHandshake);
    expect(() => decodeHandshake('{"port":1,"token":""}')).toThrow(
      MalformedHandshake,
    );
  });

  test("a port outside the range is refused", () => {
    expect(() => decodeHandshake('{"port":70000,"token":"t"}')).toThrow(
      MalformedHandshake,
    );
    expect(() => decodeHandshake('{"port":1.5,"token":"t"}')).toThrow(
      MalformedHandshake,
    );
  });

  test("anything that is not the handshake is refused, with the line in hand", () => {
    for (const line of ["", "hello", "null", "[]", '"port"', "{"]) {
      expect(() => decodeHandshake(line)).toThrow(MalformedHandshake);
    }
    try {
      decodeHandshake("not json");
    } catch (error) {
      // The line goes in the message: a gateway that answered wrongly is a version
      // mismatch, and what it said is the only clue to which version.
      expect((error as Error).message).toContain("not json");
    }
  });
});
