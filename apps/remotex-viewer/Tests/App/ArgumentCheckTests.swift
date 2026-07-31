import Testing
@testable import RemotexViewer

struct ArgumentCheckTests {
    /// The failure that motivated this: a removed flag, silently ignored, sent a
    /// QA run at the real instance. Now it is named.
    @Test func aRemovedFlagIsReported() {
        #expect(
            ArgumentCheck.unknownOptions(in: ["remotex-viewer", "--settings", "qa"]) == ["--settings"]
        )
    }

    @Test func everyKnownOptionIsAccepted() {
        #expect(ArgumentCheck.unknownOptions(in: ["remotex-viewer"]).isEmpty)
        #expect(ArgumentCheck.unknownOptions(in: ["remotex-viewer", "--version"]).isEmpty)
        #expect(
            ArgumentCheck.unknownOptions(in: ["remotex-viewer", "--instance-dir", "/tmp/qa"]).isEmpty
        )
        #expect(
            ArgumentCheck.unknownOptions(in: [
                "remotex-viewer", "--probe", "--probe-target", "mac", "--probe-seconds", "30",
            ]).isEmpty
        )
    }

    /// macOS hands a GUI launch its own single-dash arguments; they are not the
    /// app's to police and must not read as a typo.
    @Test func macOSSingleDashInjectionsAreIgnored() {
        #expect(
            ArgumentCheck.unknownOptions(in: [
                "remotex-viewer", "-NSDocumentRevisionsDebugMode", "YES", "-psn_0_12345",
            ]).isEmpty
        )
    }

    /// More than one unknown option is reported together, in order.
    @Test func severalUnknownOptionsAreAllReported() {
        #expect(
            ArgumentCheck.unknownOptions(in: ["remotex-viewer", "--gateway", "x", "--settings", "y"])
                == ["--gateway", "--settings"]
        )
    }
}
