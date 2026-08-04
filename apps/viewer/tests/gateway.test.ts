// The process contract, from this end.
//
// `tests/embedded_gateway_e2e.rs` asserts it from the gateway's side against the
// real binary; these run the same shapes against fakes in `tests/fakes/`, so the two
// halves can be wrong independently rather than agreeing with each other by
// construction.
//
// Nothing here asserts a duration. A slower machine changes when these finish, not
// what they decide.

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  EmbeddedGateway,
  type LaunchFailureKind,
} from "../src/main/gateway.ts";
import { type InstanceDirectory, instanceAt } from "../src/main/instance.ts";

const fakes = join(import.meta.dir, "fakes");

let instance: InstanceDirectory;

beforeEach(() => {
  instance = instanceAt(mkdtempSync(join(tmpdir(), "remotex-viewer-")));
});

afterEach(() => {
  rmSync(instance.dir, { recursive: true, force: true });
});

function gatewayRunning(fake: string): EmbeddedGateway {
  return new EmbeddedGateway(
    instance,
    { binary: join(fakes, fake), webRoot: fakes },
    {
      // `node` runs the fake; the arguments the app passes are asserted below
      // rather than being implied by whether it started.
      spawn: (binary, args, options) =>
        spawn(process.execPath, [binary, ...args], options),
      // No log file: what these care about is the tail the launch screen shows.
      openLog: () => null,
    },
  );
}

async function failureOf(gateway: EmbeddedGateway): Promise<{
  kind: LaunchFailureKind;
  message: string;
  log: string;
}> {
  try {
    await gateway.start();
  } catch (error) {
    const failure = error as {
      kind: LaunchFailureKind;
      message: string;
      log: string;
    };
    return { kind: failure.kind, message: failure.message, log: failure.log };
  }
  throw new Error("that gateway started");
}

describe("starting", () => {
  test("the handshake is the port and the token, and nothing was guessed", async () => {
    const gateway = gatewayRunning("good-gateway.js");
    const handshake = await gateway.start();
    expect(handshake).toEqual({ port: 49213, token: "a-token" });
    await gateway.stop();
  });

  test("the arguments are the two the gateway will accept", async () => {
    // `src/cli.rs` refuses --port, --gateway and --token by design: the port and the
    // secret are the gateway's to decide. Passing either would be a start that fails
    // for a reason nothing on screen could explain.
    const seen: string[][] = [];
    const gateway = new EmbeddedGateway(
      instance,
      { binary: join(fakes, "good-gateway.js"), webRoot: fakes },
      {
        spawn: (binary, args, options) => {
          seen.push(args);
          return spawn(process.execPath, [binary, ...args], options);
        },
        openLog: () => null,
      },
    );
    await gateway.start();
    expect(seen[0]).toEqual([
      "serve-embedded",
      "--instance-dir",
      instance.dir,
      "--web-root",
      fakes,
    ]);
    await gateway.stop();
  });

  test("a missing gateway is named as a broken bundle, not as a failed start", async () => {
    const gateway = new EmbeddedGateway(instance, {
      binary: join(fakes, "no-such-binary"),
      webRoot: fakes,
    });
    const failure = await failureOf(gateway);
    expect(failure.kind).toBe("executableMissing");
  });

  test("a missing client is a different broken bundle", async () => {
    // The two halves are copied in by different steps of the build, and either can
    // be the one that failed.
    const gateway = new EmbeddedGateway(instance, {
      binary: join(fakes, "good-gateway.js"),
      webRoot: join(fakes, "no-such-directory"),
    });
    const failure = await failureOf(gateway);
    expect(failure.kind).toBe("clientMissing");
  });
});

describe("failing", () => {
  test("a gateway that says nothing is a different failure from one that refuses", async () => {
    // No message to show, and a different answer: this one is a bug or a wedged
    // machine, not a file to fix.
    const gateway = new EmbeddedGateway(
      instance,
      { binary: join(fakes, "silent-gateway.js"), webRoot: fakes },
      {
        spawn: (binary, args, options) =>
          spawn(process.execPath, [binary, ...args], options),
        openLog: () => null,
        handshakeTimeoutMs: 200,
      },
    );
    const failure = await failureOf(gateway);
    expect(failure.kind).toBe("silent");
    expect(failure.log).toBe("");
  });

  test("a refused config is reported in the gateway's own words", async () => {
    // Never summarised: it names the block that is wrong, and no sentence written
    // here would be more use than that one.
    const failure = await failureOf(gatewayRunning("refusing-gateway.js"));
    expect(failure.kind).toBe("refused");
    expect(failure.message).toContain("[server]");
    expect(failure.log).toContain("[server]");
  });

  test("a gateway that answers with something else is a version disagreement", async () => {
    const failure = await failureOf(gatewayRunning("babbling-gateway.js"));
    expect(failure.kind).toBe("malformedHandshake");
    expect(failure.message).toContain("remotex 0.0.1");
  });
});

describe("stopping", () => {
  test("closing the pipe is enough, even for a gateway that ignores signals", async () => {
    // The point of the arrangement: the guarantee needs no code of ours to run at
    // the right moment, so a gateway that catches SIGTERM still dies with the app.
    const gateway = gatewayRunning("deaf-gateway.js");
    await gateway.start();
    expect(gateway.isRunning()).toBe(true);
    await gateway.stop();
    expect(gateway.isRunning()).toBe(false);
  });

  test("a stop that was asked for is not reported as a death", async () => {
    // The bug this is here for: a restart racing the exit it caused, turning into
    // an error screen over a gateway that was told to go.
    const gateway = gatewayRunning("good-gateway.js");
    let died = 0;
    gateway.onUnexpectedExit = () => {
      died += 1;
    };
    await gateway.start();
    await gateway.stop();
    await new Promise((done) => setTimeout(done, 400));
    expect(died).toBe(0);
  });

  test("a gateway that dies on its own is reported once", async () => {
    const gateway = gatewayRunning("good-gateway.js");
    const deaths: string[] = [];
    gateway.onUnexpectedExit = (failure) => deaths.push(failure.kind);
    await gateway.start();
    process.kill(
      // The child is the fake's own process; killing it is the "died unasked" case.
      (gateway as unknown as { child: { pid: number } }).child.pid,
      "SIGKILL",
    );
    await new Promise((done) => setTimeout(done, 600));
    expect(deaths).toEqual(["refused"]);
  });
});
