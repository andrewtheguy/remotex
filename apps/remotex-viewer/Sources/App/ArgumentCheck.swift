import Foundation

/// The command-line options this app understands, and a refusal for anything
/// else.
///
/// The whole reason this exists: `--settings qa` (a flag removed long ago) was
/// silently ignored, so the launch fell through to the *real* instance instead
/// of a throwaway one — a QA run pointed at live data with no warning. An
/// unrecognised `--option` is now refused out loud rather than dropped.
enum ArgumentCheck {
    /// Every double-dash option the app accepts. Value-taking flags
    /// (`--instance-dir <dir>`, `--probe-target <name>`, `--probe-seconds <n>`)
    /// are listed by the flag itself; their values are ordinary tokens and are
    /// not judged here.
    ///
    /// Spelled as literals rather than referencing `InstanceDirectory.flag` /
    /// `ProbeCommand.flag`: those are `@MainActor`-isolated, and a nonisolated
    /// static default cannot read them. Keep this list in step with them.
    static let known: Set<String> = [
        "--version",
        "--instance-dir",
        "--probe",
        "--probe-target",
        "--probe-seconds",
    ]

    /// The unrecognised options in `arguments` (argv[0], the executable path, is
    /// skipped).
    ///
    /// Only **double-dash** tokens are judged. macOS injects its own single-dash
    /// arguments into a GUI launch — `-NSDocumentRevisionsDebugMode`, `-psn_…`,
    /// `-AppleLanguages` — which are none of this app's business and must not be
    /// mistaken for a user's typo.
    static func unknownOptions(in arguments: [String]) -> [String] {
        arguments.dropFirst().filter { $0.hasPrefix("--") && !known.contains($0) }
    }

    /// Refuse unknown options with a usage line and a non-zero exit, or return so
    /// startup can continue. Called before anything else in `ViewerMain.main`.
    static func enforce(_ arguments: [String] = CommandLine.arguments) {
        let unknown = unknownOptions(in: arguments)
        guard !unknown.isEmpty else { return }
        let plural = unknown.count == 1 ? "" : "s"
        FileHandle.standardError.write(Data(
            """
            remotex-viewer: unknown option\(plural): \(unknown.joined(separator: " "))
            usage: remotex-viewer [--instance-dir <dir>] \
            [--probe [--probe-target <name>] [--probe-seconds <n>]] [--version]

            """.utf8
        ))
        Foundation.exit(2)
    }
}
