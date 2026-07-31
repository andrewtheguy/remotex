import Foundation
@testable import RemotexViewer

/// A directory that removes itself, for the tests whose subject is a file.
///
/// The instance directory is where this app keeps everything, so several suites need
/// a throwaway one. Hand-rolled the same way the gateway's tests do it
/// (`tests/common/mod.rs`), and keyed on a UUID because more than one of these exists
/// at a time and two that turned out to be the same directory would quietly share a
/// config.
final class ScratchDirectory {
    let url: URL

    init(_ tag: String = "instance") throws {
        url = URL.temporaryDirectory
            .appending(path: "remotex-tests-\(tag)-\(UUID().uuidString)", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    }

    /// The instance directory this scratch space stands in for.
    var instance: InstanceDirectory {
        InstanceDirectory(url: url)
    }

    /// Write `contents` to `name` inside the directory, returning where it went.
    @discardableResult
    func write(_ name: String, _ contents: String) throws -> URL {
        let file = url.appending(path: name)
        try Data(contents.utf8).write(to: file, options: .atomic)
        return file
    }

    /// What is in `name`, or nil if it is not there — which is the assertion a
    /// refused save needs.
    func contents(of name: String) -> String? {
        try? String(contentsOf: url.appending(path: name), encoding: .utf8)
    }

    deinit {
        try? FileManager.default.removeItem(at: url)
    }
}
