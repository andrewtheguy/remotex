import { describe, expect, test } from "bun:test";
import { instanceAt, instanceDirFromArgv } from "../src/main/instance.ts";

const argv = (...rest: string[]) => ["/Applications/remotex.app", ...rest];

describe("--instance-dir", () => {
  test("no flag means the default, which is the caller's to decide", () => {
    expect(instanceDirFromArgv(argv(), "/work", "/Users/a")).toBeNull();
  });

  test("an absolute path is taken as it stands", () => {
    expect(
      instanceDirFromArgv(
        argv("--instance-dir", "/tmp/qa"),
        "/work",
        "/Users/a",
      ),
    ).toBe("/tmp/qa");
  });

  test("a relative path is resolved against the working directory", () => {
    // An app launched by `open` inherits `/`, so a relative path resolved later
    // would land somewhere nobody meant.
    expect(
      instanceDirFromArgv(
        argv("--instance-dir", "tmp/qa"),
        "/work",
        "/Users/a",
      ),
    ).toBe("/work/tmp/qa");
  });

  test("a tilde is expanded", () => {
    expect(
      instanceDirFromArgv(argv("--instance-dir", "~/qa"), "/work", "/Users/a"),
    ).toBe("/Users/a/qa");
    expect(
      instanceDirFromArgv(argv("--instance-dir", "~"), "/work", "/Users/a"),
    ).toBe("/Users/a");
  });

  test("the equals spelling is the same flag", () => {
    // The form somebody types. Missing it would not read as a broken flag — it
    // would read as a QA run that quietly used the real instance.
    expect(
      instanceDirFromArgv(argv("--instance-dir=/tmp/qa"), "/work", "/Users/a"),
    ).toBe("/tmp/qa");
    expect(
      instanceDirFromArgv(argv("--instance-dir=~/qa"), "/work", "/Users/a"),
    ).toBe("/Users/a/qa");
  });

  test("an equals with nothing after it is refused like the other spelling", () => {
    const said: string[] = [];
    expect(
      instanceDirFromArgv(argv("--instance-dir="), "/work", "/Users/a", (m) =>
        said.push(m),
      ),
    ).toBeNull();
    expect(said).toHaveLength(1);
  });

  test("a flag with nothing after it is refused out loud", () => {
    // Falling back silently is the wrong direction for a flag whose whole purpose
    // is keeping a test run away from the real instance.
    const said: string[] = [];
    expect(
      instanceDirFromArgv(argv("--instance-dir"), "/work", "/Users/a", (m) =>
        said.push(m),
      ),
    ).toBeNull();
    expect(said).toHaveLength(1);
    expect(said[0]).toContain("--instance-dir");
  });

  test("another flag is not a path", () => {
    const said: string[] = [];
    expect(
      instanceDirFromArgv(
        argv("--instance-dir", "--probe"),
        "/work",
        "/Users/a",
        (m) => said.push(m),
      ),
    ).toBeNull();
    expect(said).toHaveLength(1);
  });
});

describe("the instance directory", () => {
  test("holds the config, the log and the profile, and nothing else", () => {
    const instance = instanceAt("/tmp/qa");
    expect(instance.configPath).toBe("/tmp/qa/remotex.toml");
    expect(instance.logPath).toBe("/tmp/qa/gateway.log");
    // Inside the instance, which is what makes --instance-dir isolate the client's
    // remembered preferences the same way it isolates the config.
    expect(instance.profileDir).toBe("/tmp/qa/electron");
  });
});
