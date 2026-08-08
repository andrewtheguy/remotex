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
  // Build metadata first, then the pre-release tag: semver orders them
  // `1.2.3-rc.1+build.5`, so stripping `-` first would leave `+build.5` behind on a
  // version carrying both.
  const parts = cargo.split("+")[0].split("-")[0].split(".").slice(0, 4);
  if (!parts.every(isField)) {
    throw new Error(`cannot turn "${cargo}" into a Chrome version`);
  }
  return { version: parts.map(Number).join("."), version_name: cargo };
}

/**
 * Digits and nothing else, then the range.
 *
 * Tested as a string rather than through `parseInt`, which stops reading at the first
 * character it dislikes: it turns `1x` into `1`, and would have shipped a manifest
 * claiming a version nobody wrote.
 */
function isField(part: string): boolean {
  return /^\d+$/.test(part) && Number(part) <= 65_535;
}
