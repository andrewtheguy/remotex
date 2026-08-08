// The workspace version, as Chrome will accept it.
//
// Pure, and separated from the build script so it can be tested without a filesystem.

/**
 * Chrome's `version` is one to four integers in 0–65535, separated by dots, and
 * nothing else. The workspace version is semver, which may carry a pre-release tag —
 * `0.9.0-rc.1` is a perfectly good Cargo version and an illegal Chrome one.
 *
 * So the numeric head becomes `version`, and the whole string becomes `version_name`,
 * which Chrome shows to the user and does not parse. Nothing is lost and nothing is
 * invented.
 */
export function chromeVersion(cargo: string): {
  version: string;
  version_name: string;
} {
  const parts = cargo.split("-")[0].split(".").slice(0, 4);
  const numbers = parts.map((part) => Number.parseInt(part, 10));
  if (numbers.length === 0 || numbers.some((n) => !isField(n))) {
    throw new Error(`cannot turn "${cargo}" into a Chrome version`);
  }
  return { version: numbers.join("."), version_name: cargo };
}

function isField(value: number): boolean {
  return Number.isInteger(value) && value >= 0 && value <= 65_535;
}
