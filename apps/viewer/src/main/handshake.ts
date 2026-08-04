// The one line the gateway prints, and what it has to say before this app will
// believe it.
//
// The other end is `Handshake` in `src/embedded.rs`, which writes exactly this and
// then nothing else to stdout ever again. Kept pure and apart from the spawning so
// the shapes that are worth testing — junk, a truncated line, a port of zero — can
// be tested without a process.

export interface Handshake {
  /** The loopback port the kernel gave the gateway. */
  port: number;
  /** The launch token, which goes into the cookie jar and nowhere else. */
  token: string;
}

export class MalformedHandshake extends Error {
  constructor(readonly line: string) {
    super(
      `The local gateway said something this app could not read: ${
        line.length > 200 ? `${line.slice(0, 200)}…` : line
      }`,
    );
    this.name = "MalformedHandshake";
  }
}

/**
 * Decode one handshake line, or throw {@link MalformedHandshake}.
 *
 * A port of zero is refused rather than passed on: zero is what the gateway was
 * *asked* to bind, so reading it back means the port was never resolved, and a
 * request to `http://127.0.0.1:0` fails in a way that looks like a network problem
 * rather than like this.
 */
export function decodeHandshake(line: string): Handshake {
  let parsed: unknown;
  try {
    parsed = JSON.parse(line);
  } catch {
    throw new MalformedHandshake(line);
  }
  if (typeof parsed !== "object" || parsed === null) {
    throw new MalformedHandshake(line);
  }
  const { port, token } = parsed as { port?: unknown; token?: unknown };
  if (
    typeof port !== "number" ||
    !Number.isInteger(port) ||
    port <= 0 ||
    port > 65535 ||
    typeof token !== "string" ||
    token === ""
  ) {
    throw new MalformedHandshake(line);
  }
  return { port, token };
}
