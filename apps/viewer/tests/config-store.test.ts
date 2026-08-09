// What the configuration editor does with an answer from `check-config --embedded`.
//
// The validator is injected, so these are about the *decision* — refuse and write
// nothing, or accept and write atomically — rather than about the gateway's rules,
// which are the gateway's to test and are tested there.

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  ConfigStore,
  TEMPLATE,
  validatorOverBinary,
} from "../src/main/config-store.ts";
import { type InstanceDirectory, instanceAt } from "../src/main/instance.ts";

let instance: InstanceDirectory;

beforeEach(() => {
  instance = instanceAt(mkdtempSync(join(tmpdir(), "remotex-config-")));
});

afterEach(() => {
  rmSync(instance.dir, { recursive: true, force: true });
});

const accepts = () => Promise.resolve({ ok: true as const });
const refuses = (message: string) => () =>
  Promise.resolve({ ok: false as const, error: message });

/** Whatever a write left behind. The directory is documented as holding three. */
const leftovers = () =>
  readdirSync(instance.dir).filter((name) => name.endsWith(".new"));

/** Past every pending microtask, so "has it started yet" is a settled question. */
const flush = () => new Promise((done) => setImmediate(done));

describe("reading", () => {
  test("a first launch gets the template, and the template has no targets", () => {
    // An example target that *looks* configured is worse than none: the picker
    // would offer a machine that does not exist.
    const store = new ConfigStore(instance, accepts);
    expect(store.read()).toBe(TEMPLATE);
    expect(TEMPLATE).not.toMatch(/^\[\[targets]]/m);
    expect(TEMPLATE).toContain("# [[targets]]");
  });

  test("bootstrapping writes the template once and never over an edit", () => {
    const store = new ConfigStore(instance, accepts);
    store.bootstrapIfNeeded();
    writeFileSync(instance.configPath, '[branding]\ntext = "mine"\n');
    store.bootstrapIfNeeded();
    expect(store.read()).toBe('[branding]\ntext = "mine"\n');
  });
});

describe("saving", () => {
  test("a refusal writes nothing at all", async () => {
    // Not a backup, not a partial file. The config on disk is always one the gateway
    // starts on, so a rejected edit leaves the app exactly as usable as it was.
    const store = new ConfigStore(
      instance,
      refuses("error: this config may not have a [server] block"),
    );
    store.bootstrapIfNeeded();
    const before = readFileSync(instance.configPath, "utf8");

    const result = await store.save("[server]\nport = 1\n");
    expect(result.ok).toBe(false);
    expect(readFileSync(instance.configPath, "utf8")).toBe(before);
    expect(leftovers()).toEqual([]);
  });

  test("the complaint is passed through untouched", async () => {
    const store = new ConfigStore(
      instance,
      refuses("error: target 'work' has no host"),
    );
    const result = await store.save("nonsense");
    expect(result).toEqual({
      ok: false,
      error: "error: target 'work' has no host",
    });
  });

  test("an accepted config is written owner-only", async () => {
    // The file holds every target's credentials.
    const store = new ConfigStore(instance, accepts);
    const result = await store.save('[branding]\ntext = "work"\n');
    expect(result.ok).toBe(true);
    expect(readFileSync(instance.configPath, "utf8")).toBe(
      '[branding]\ntext = "work"\n',
    );
    expect(statSync(instance.configPath).mode & 0o777).toBe(0o600);
    expect(leftovers()).toEqual([]);
  });

  test("a second save waits for the first, check and write together", async () => {
    // A check takes as long as the gateway takes, so two Saves can be outstanding
    // at once. Interleaved they are two staging files with one name, and the file on
    // disk is whichever rename happened to land second — not the edit last pressed.
    const started: string[] = [];
    const gates: ((result: { ok: true }) => void)[] = [];
    const store = new ConfigStore(instance, (text) => {
      started.push(text);
      return new Promise((resolve) => gates.push(resolve));
    });

    const first = store.save("first\n");
    const second = store.save("second\n");
    await flush();
    expect(started).toEqual(["first\n"]);

    gates[0]({ ok: true });
    expect(await first).toEqual({ ok: true });
    await flush();
    expect(started).toEqual(["first\n", "second\n"]);

    gates[1]({ ok: true });
    expect(await second).toEqual({ ok: true });
    expect(readFileSync(instance.configPath, "utf8")).toBe("second\n");
    expect(leftovers()).toEqual([]);
  });
});

describe("asking the gateway", () => {
  test("a binary that is not there is a failure, not a crash", async () => {
    const validate = validatorOverBinary(join(instance.dir, "no-such-binary"));
    const result = await validate('[branding]\ntext = "x"\n');
    expect(result.ok).toBe(false);
  });

  test("a non-zero exit is the gateway's own stderr", async () => {
    // Written as a shell script rather than mocked, because the thing being checked
    // is that stdin is closed and stderr is read — a real child is the only way to
    // find out that it was not.
    const fake = join(instance.dir, "fake-gateway.sh");
    writeFileSync(
      fake,
      "#!/bin/sh\ncat > /dev/null\necho 'error: no [[targets]]' >&2\nexit 1\n",
      { mode: 0o755 },
    );
    const result = await validatorOverBinary(fake)("");
    expect(result).toEqual({ ok: false, error: "error: no [[targets]]" });
  });

  test("a zero exit is an accepted config", async () => {
    const fake = join(instance.dir, "ok-gateway.sh");
    writeFileSync(fake, "#!/bin/sh\ncat > /dev/null\nexit 0\n", {
      mode: 0o755,
    });
    expect(await validatorOverBinary(fake)('[branding]\ntext = "x"\n')).toEqual(
      {
        ok: true,
      },
    );
  });
});
