// The gateway process this app runs, and the promise that it stops when the app
// does.
//
// Two pipes, in opposite directions, doing two unrelated jobs — the same pair
// described from the other side in `src/embedded.rs`:
//
// - **its stdout → us**, once: the handshake line, printed after the gateway has
//   bound its socket. It is how the port and the token are learned, and the reason
//   neither has to be guessed or passed in.
// - **us → its stdin**, never: nothing is written on it in either direction. It is
//   open for as long as this process lives, so when this process ends — cleanly,
//   crashed, Force Quit, `kill -9` — the kernel closes our end, the gateway reads
//   end-of-file and exits. That is the layer the "no stray gateway" guarantee rests
//   on, because it needs no code of ours to run at the right moment.
//
// `stop()` is the polite path on top of it, not instead of it.

import {
  type ChildProcess,
  type SpawnOptions,
  spawn,
} from "node:child_process";
import { createWriteStream, existsSync, type WriteStream } from "node:fs";
import {
  decodeHandshake,
  type Handshake,
  MalformedHandshake,
} from "./handshake.ts";
import type { InstanceDirectory } from "./instance.ts";
import { LogTail } from "./logtail.ts";

/**
 * How long to wait for the handshake. Generous: it covers a cold start of a
 * notarized binary whose signature Gatekeeper is checking for the first time,
 * which is slow once and instant afterwards.
 */
export const HANDSHAKE_TIMEOUT_MS = 20_000;

/** How long a terminated gateway gets to go quietly before it is killed. */
export const TERMINATION_GRACE_MS = 1500;

/**
 * How long a failing gateway gets to finish its sentence.
 *
 * A dying gateway writes its explanation to stderr and then exits, and the two
 * pipes close independently: stdout's end-of-file can be delivered first, and a
 * failure reported at that instant would carry an empty log for a gateway that
 * *did* say why. This is not a timing assumption about how fast anything is — the
 * wait ends the moment stderr closes, and this is only the ceiling on it.
 */
export const DRAIN_GRACE_MS = 300;

/**
 * Why a launch did not produce a serving gateway.
 *
 * Each kind exists because it needs a different sentence in front of somebody: a
 * broken bundle is not a broken config, and neither is a gateway that started and
 * then died.
 */
export type LaunchFailureKind =
  /** No `remotex-gateway` in this bundle — an unbundled or half-built app. */
  | "executableMissing"
  /**
   * No client in this bundle. Distinct from the above because the two halves are
   * copied in by different steps of the build, and either can be the one that
   * failed.
   */
  | "clientMissing"
  /** The instance directory could not be created or read. */
  | "instanceUnavailable"
  /** The process could not be started at all (permissions, a bad architecture). */
  | "notStarted"
  /**
   * It ran and exited without serving. The output is its own explanation, which
   * for the common case is a config it refused.
   */
  | "refused"
  /**
   * It is running but said nothing in time. Distinguished from `refused` because
   * there is no message to show and the answer is different: this one is a bug or
   * a wedged machine, not a file to fix.
   */
  | "silent"
  /**
   * It printed something that is not a handshake — a version mismatch between the
   * app and the binary it carries.
   */
  | "malformedHandshake";

export class LaunchFailure extends Error {
  constructor(
    readonly kind: LaunchFailureKind,
    message: string,
    /** The gateway's own stderr, verbatim. Empty when it said nothing. */
    readonly log = "",
  ) {
    super(message);
    this.name = "LaunchFailure";
  }

  static executableMissing(): LaunchFailure {
    return new LaunchFailure(
      "executableMissing",
      "This copy of remotex is incomplete: it has no gateway to run.",
    );
  }

  static clientMissing(): LaunchFailure {
    return new LaunchFailure(
      "clientMissing",
      "This copy of remotex is incomplete: it has no client to show.",
    );
  }

  static instanceUnavailable(reason: string): LaunchFailure {
    return new LaunchFailure(
      "instanceUnavailable",
      `The remotex folder could not be opened: ${reason}`,
    );
  }
}

export interface GatewayPaths {
  /** `Contents/Resources/remotex-gateway`, or the built binary in a dev tree. */
  binary: string;
  /** `Contents/Resources/web`, the SPA handed over as `--web-root`. */
  webRoot: string;
}

/**
 * How the gateway is started — narrower than node's own `spawn`, which has ten
 * overloads and cannot be substituted for in a test without naming all of them.
 * This is the one shape the gateway actually asks for.
 */
export type SpawnLike = (
  binary: string,
  args: string[],
  options: SpawnOptions,
) => ChildProcess;

/** Injected so tests can run a gateway against a fake. */
export interface GatewayHooks {
  spawn: SpawnLike;
  /** Opens the append stream for `gateway.log`. Skipped entirely in tests. */
  openLog?: (path: string) => WriteStream | null;
  exists: (path: string) => boolean;
  /**
   * How long to wait for the handshake.
   *
   * Injected only so the "said nothing" case can be asserted at all — waiting out
   * the real twenty seconds would make it a test nobody runs. Note what this is
   * *not*: no other test here has a duration in it, because a slower machine must
   * change when they finish and not what they decide.
   */
  handshakeTimeoutMs: number;
}

const defaultHooks: GatewayHooks = {
  spawn,
  openLog: (path) => createWriteStream(path, { flags: "a" }),
  exists: existsSync,
  handshakeTimeoutMs: HANDSHAKE_TIMEOUT_MS,
};

export class EmbeddedGateway {
  private child: ChildProcess | null = null;
  private logFile: WriteStream | null = null;
  private readonly tail = new LogTail();
  /** True while `stop()` is the reason the process is ending. */
  private stopping = false;
  private stderrClosed = false;
  private stderrClosedWaiters: (() => void)[] = [];
  private readonly hooks: GatewayHooks;

  /**
   * Called when the gateway exits while the app is still using it — a failure the
   * app has to show rather than discover on the next request.
   */
  onUnexpectedExit: ((failure: LaunchFailure) => void) | null = null;

  constructor(
    private readonly instance: InstanceDirectory,
    private readonly paths: GatewayPaths,
    hooks: Partial<GatewayHooks> = {},
  ) {
    this.hooks = { ...defaultHooks, ...hooks };
  }

  /** What the gateway has said this launch. The launch screen shows it verbatim. */
  log(): string {
    return this.tail.text();
  }

  isRunning(): boolean {
    return this.child !== null && this.child.exitCode === null;
  }

  /**
   * Start it and wait for the one line it prints.
   *
   * Whichever of {handshake, stdout end-of-file, timeout} happens first settles
   * this, and the rest are ignored — "exactly once" being a fact here rather than
   * an intention.
   */
  async start(): Promise<Handshake> {
    if (!this.hooks.exists(this.paths.binary)) {
      throw LaunchFailure.executableMissing();
    }
    if (!this.hooks.exists(this.paths.webRoot)) {
      throw LaunchFailure.clientMissing();
    }
    this.tail.clear();
    this.stopping = false;
    this.stderrClosed = false;

    const child = this.hooks.spawn(
      this.paths.binary,
      [
        "serve-embedded",
        "--instance-dir",
        this.instance.dir,
        "--web-root",
        this.paths.webRoot,
      ],
      // Three pipes, and every one of them load-bearing. Not `inherit`: stdin is
      // the liveness pipe and must be ours to hold open.
      { stdio: ["pipe", "pipe", "pipe"] },
    );
    this.child = child;
    this.logFile = this.hooks.openLog?.(this.instance.logPath) ?? null;

    return await new Promise<Handshake>((settle, refuse) => {
      let settled = false;
      const finish = (outcome: () => void) => {
        if (settled) {
          return;
        }
        settled = true;
        clearTimeout(timer);
        outcome();
      };

      const timer = setTimeout(() => {
        finish(() => {
          void this.stop();
          refuse(
            new LaunchFailure(
              "silent",
              "The local gateway did not answer. Try again.",
              this.tail.text(),
            ),
          );
        });
      }, this.hooks.handshakeTimeoutMs);

      child.on("error", (error: Error) => {
        finish(() =>
          refuse(
            new LaunchFailure(
              "notStarted",
              `The local gateway could not be started: ${error.message}`,
            ),
          ),
        );
      });

      let line = "";
      child.stdout?.setEncoding("utf8");
      child.stdout?.on("data", (chunk: string) => {
        if (settled) {
          // Nothing follows the handshake on this pipe, so anything here is a
          // build that disagrees with this one. Keep it out of the log file, which
          // is stderr's, and out of the way.
          return;
        }
        line += chunk;
        const end = line.indexOf("\n");
        if (end === -1) {
          return;
        }
        const first = line.slice(0, end);
        finish(() => {
          try {
            settle(decodeHandshake(first));
          } catch (error) {
            void this.stop();
            refuse(
              new LaunchFailure(
                "malformedHandshake",
                error instanceof MalformedHandshake
                  ? error.message
                  : String(error),
                this.tail.text(),
              ),
            );
          }
        });
      });

      child.stderr?.setEncoding("utf8");
      child.stderr?.on("data", (chunk: string) => {
        this.tail.push(chunk);
        this.logFile?.write(chunk);
      });
      child.stderr?.on("end", () => {
        this.stderrClosed = true;
        for (const wake of this.stderrClosedWaiters.splice(0)) {
          wake();
        }
      });

      child.on("exit", (code, signal) => {
        this.logFile?.end();
        this.logFile = null;
        const wasStopping = this.stopping;
        void this.drained().then(() => {
          if (wasStopping || this.stopping) {
            // Asked for. The re-check after the drain is deliberate: a restart can
            // begin while stderr is still closing, and reporting a death that was
            // requested is how a restart turns into an error screen.
            return;
          }
          const failure = new LaunchFailure(
            "refused",
            this.tail.isEmpty()
              ? `The local gateway stopped without saying why (${
                  signal ?? `exit ${code}`
                }).`
              : this.tail.text(),
            this.tail.text(),
          );
          finish(() => refuse(failure));
          // Reported as well as refused: after a successful start there is nobody
          // waiting on the promise, and this is the only way the app hears about it.
          this.onUnexpectedExit?.(failure);
        });
      });
    });
  }

  /**
   * Stop it: close the pipe first, then ask, then insist.
   *
   * The order is the design. Closing stdin is what the gateway is actually written
   * to notice, and it is the only step that also works when this process is not the
   * one doing the stopping.
   */
  async stop(): Promise<void> {
    const child = this.child;
    if (!child) {
      return;
    }
    this.stopping = true;
    this.child = null;
    if (child.exitCode !== null) {
      return;
    }
    child.stdin?.end();
    child.kill("SIGTERM");
    await new Promise<void>((done) => {
      const timer = setTimeout(() => {
        child.kill("SIGKILL");
        done();
      }, TERMINATION_GRACE_MS);
      child.once("exit", () => {
        clearTimeout(timer);
        done();
      });
    });
  }

  /**
   * Resolve once the gateway has finished saying whatever it was saying, or once
   * {@link DRAIN_GRACE_MS} has passed — whichever comes first.
   */
  private drained(): Promise<void> {
    if (this.stderrClosed) {
      return Promise.resolve();
    }
    return new Promise<void>((done) => {
      const timer = setTimeout(done, DRAIN_GRACE_MS);
      this.stderrClosedWaiters.push(() => {
        clearTimeout(timer);
        done();
      });
    });
  }
}
