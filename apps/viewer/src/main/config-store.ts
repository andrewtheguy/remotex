// The instance's `remotex.toml`: read it, check it, write it.
//
// Checking is delegated to the gateway binary — `check-config --embedded` — and that
// is the whole design. What the editor accepts is by construction what the gateway
// accepts, and there is no idea in this tree of what a config *means* that could
// drift from the Rust one. There is no TOML parser here and there must never be.

import { spawn } from "node:child_process";
import {
  chmodSync,
  existsSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import type { InstanceDirectory } from "./instance.ts";

/** How long a check gets before it is treated as an ordinary failure. */
export const CHECK_TIMEOUT_MS = 15_000;

export type CheckResult = { ok: true } | { ok: false; error: string };

/** Injected so the editor's behaviour can be tested without a built binary. */
export type Validator = (text: string) => Promise<CheckResult>;

/**
 * Run `check-config --embedded` over candidate text.
 *
 * Both pipes are drained before stdin is written and closed: a child that fills its
 * stderr buffer while nobody is reading it blocks, and this would then wait out its
 * watchdog for a config it had already refused.
 *
 * The complaint is passed through untouched. `check-config` already names the file,
 * the target and the key, and a friendlier sentence in front of it would only hide
 * the part that says what to fix.
 */
export function validatorOverBinary(binary: string): Validator {
  return (text) =>
    new Promise<CheckResult>((done) => {
      const child = spawn(binary, ["check-config", "--embedded"], {
        stdio: ["pipe", "pipe", "pipe"],
      });
      let stderr = "";
      let settled = false;
      const settle = (result: CheckResult) => {
        if (settled) {
          return;
        }
        settled = true;
        clearTimeout(timer);
        done(result);
      };
      const timer = setTimeout(() => {
        child.kill("SIGKILL");
        settle({
          ok: false,
          error:
            "The configuration could not be checked: the gateway did not answer.",
        });
      }, CHECK_TIMEOUT_MS);

      child.stderr?.setEncoding("utf8");
      child.stderr?.on("data", (chunk: string) => {
        stderr += chunk;
      });
      child.stdout?.resume();
      child.on("error", (error: Error) => {
        settle({
          ok: false,
          error: `The configuration could not be checked: ${error.message}`,
        });
      });
      // `close` rather than `exit`: the two are not the same moment, and the
      // difference is the whole message. `exit` fires when the process ends, which
      // can be before its stderr has been drained — so a refusal read there is a
      // refusal with the reason missing.
      child.on("close", (code) => {
        settle(
          code === 0
            ? { ok: true }
            : {
                ok: false,
                error:
                  stderr.trim() === ""
                    ? "The gateway refused this configuration without saying why."
                    : stderr.trim(),
              },
        );
      });
      child.stdin?.end(text);
    });
}

export class ConfigStore {
  /**
   * The tail of the save queue. A check takes as long as the gateway takes, so two
   * saves can be in flight at once — and interleaved they are two staging files
   * with one name, where the loser's text can land on top of the winner's. Queued
   * whole, check and write together, so the last Save pressed is the file on disk.
   */
  private saving: Promise<unknown> = Promise.resolve();
  /** Distinguishes one staging file from another; see {@link write}. */
  private writes = 0;

  constructor(
    private readonly instance: InstanceDirectory,
    private readonly validate: Validator,
  ) {}

  /** The current text, or the template when there is no file yet. */
  read(): string {
    try {
      return readFileSync(this.instance.configPath, "utf8");
    } catch {
      return TEMPLATE;
    }
  }

  /**
   * Make sure there is a config to serve.
   *
   * A first launch has no targets, which is a valid config the picker reports as an
   * empty list — so this only has to put the template where no file exists.
   */
  bootstrapIfNeeded(): void {
    if (!existsSync(this.instance.configPath)) {
      this.write(TEMPLATE);
    }
  }

  /**
   * Check `text`, and write it only if the gateway would serve it.
   *
   * Nothing is written on a refusal — not a backup, not a partial file. The point of
   * checking first is that the config on disk is always one the gateway starts on,
   * so a rejected edit leaves the app exactly as usable as it was.
   */
  save(text: string): Promise<CheckResult> {
    const queued = this.saving
      // A failed save must not wedge the queue: the next one is a fresh attempt.
      .catch(() => {})
      .then(() => this.checkAndWrite(text));
    this.saving = queued;
    return queued;
  }

  private async checkAndWrite(text: string): Promise<CheckResult> {
    const checked = await this.validate(text);
    if (!checked.ok) {
      return checked;
    }
    try {
      this.write(text);
      return { ok: true };
    } catch (error) {
      return {
        ok: false,
        error: `The configuration could not be saved: ${
          error instanceof Error ? error.message : String(error)
        }`,
      };
    }
  }

  /**
   * Write atomically at `0600`.
   *
   * Atomically because this file holds every target's credentials and the gateway
   * may be restarted the instant it changes: a half-written config is one the gateway
   * refuses, and the way never to have one is never to have it visible. `0600` for
   * the same reason the directory is `0700`.
   *
   * The staging name is this write's own — the rename is what has to be atomic, and
   * a name two writes can share is a way for one of them to be renamed holding the
   * other's text.
   */
  private write(text: string): void {
    this.writes += 1;
    const temporary = `${this.instance.configPath}.${process.pid}-${this.writes}.new`;
    try {
      writeFileSync(temporary, text, { mode: 0o600 });
      chmodSync(temporary, 0o600);
      renameSync(temporary, this.instance.configPath);
    } finally {
      // A `remotex.toml.*.new` left behind by a failed write is litter in a directory
      // documented as holding three files, and the next save would be writing over
      // somebody's unexplained leftovers.
      rmSync(temporary, { force: true });
    }
  }
}

/**
 * What a first launch gets: no targets, and a commented example of each protocol.
 *
 * Commented rather than filled in, because an example target that *looks* configured
 * is worse than none — the picker would offer a machine that does not exist. With
 * everything commented out the gateway serves an empty list and the picker says so
 * in words.
 */
export const TEMPLATE = `# remotex.app's own configuration.
#
# This file belongs to the app and to nothing else: it is not the installed
# server's /opt/remotex/etc/remotex.toml, and the two never read each other.
#
# There is no [server] block here. The app supplies its gateway's ephemeral
# loopback listener, authentication token and bundled web root. The window loads
# that same web root as remotex://app, so a [server] block would only contradict
# decisions the app has already made and is refused.
#
# Add one [[targets]] block per machine. Edit this from the app —
# Remote > Configuration… — which checks it before saving.

# What this instance calls itself: the heading above the target list, the
# window title, and the launch screen. Useful when two instances run at once.
# branding = "remotex"

# A Windows machine over RDP:
#
# [[targets]]
# name = "work"
# protocol = "rdp"
# host = "192.168.1.20"
# username = "andrew"
# password = "…"
# domain = "CORP"        # optional
# resize = true          # let the window drive the remote's size
# clipboard = true
# audio = true           # rdp only

# A VNC server:
#
# [[targets]]
# name = "pi"
# protocol = "vnc"
# host = "192.168.1.30"
# vnc_password = "…"     # the VNC server's own password
`;
