// Everything this launch reads or writes, in one directory.
//
// `remotex.app` carries its own gateway, so it also carries that gateway's whole
// installation: the config somebody edits, the log it writes, and the preferences
// the client keeps in `localStorage`. One directory rather than several homes,
// because a single unit of isolation is the only kind that can be swapped whole —
// and `--instance-dir` is exactly that swap, which is what a QA run uses instead of
// the real instance.
//
// **Nothing under `/opt/remotex` is ever consulted.** A Mac may run the server
// install and this app at once, and neither can change what the other does.

import { homedir } from "node:os";
import { isAbsolute, join, resolve } from "node:path";

/** The flag that names a directory other than the default one. */
export const INSTANCE_FLAG = "--instance-dir";

export interface InstanceDirectory {
  readonly dir: string;
  /** The one file a user edits. */
  readonly configPath: string;
  /** The gateway's stderr, appended across launches. */
  readonly logPath: string;
  /**
   * The browser profile: the cookie jar and `localStorage`.
   *
   * A subdirectory, which is what makes `--instance-dir` isolate the client's
   * remembered preferences the same way it isolates the config and the log. The
   * client keeps three of them there, and a profile that did not persist would
   * drop them at every launch, silently, with nothing on screen to say so.
   */
  readonly profileDir: string;
}

export function instanceAt(dir: string): InstanceDirectory {
  return {
    dir,
    configPath: join(dir, "remotex.toml"),
    logPath: join(dir, "gateway.log"),
    profileDir: join(dir, "electron"),
  };
}

/**
 * The `--instance-dir` a launch was given, if it was given one.
 *
 * Split out from everything that reads `process.argv` so the parsing can be tested:
 * a test process cannot choose its own arguments.
 *
 * `~` is expanded and a relative path is resolved here rather than at each use — an
 * app launched by `open` inherits `/` as its working directory, so a relative path
 * would otherwise land somewhere nobody meant.
 */
export function instanceDirFromArgv(
  argv: readonly string[],
  cwd: string,
  home: string = homedir(),
  warn: (message: string) => void = () => {},
): string | null {
  const index = argv.indexOf(INSTANCE_FLAG);
  if (index === -1) {
    return null;
  }
  const raw = (argv[index + 1] ?? "").trim();
  if (raw === "" || raw.startsWith("-")) {
    // A trailing `--instance-dir` with nothing after it is a mistake, and falling
    // back to the default is the wrong direction for a flag whose whole purpose is
    // to keep a test run away from the real instance. Said out loud, then refused.
    warn(`remotex: ${INSTANCE_FLAG} needs a path; using the default instance`);
    return null;
  }
  if (raw === "~") {
    return home;
  }
  if (raw.startsWith("~/")) {
    return join(home, raw.slice(2));
  }
  return isAbsolute(raw) ? resolve(raw) : resolve(cwd, raw);
}
