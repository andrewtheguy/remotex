import Foundation

/// One side of a child process's output, read as it arrives.
///
/// Event-driven (`readabilityHandler`) rather than a thread blocked in `read`, for a
/// reason that matters at launch: waiting for the handshake has a deadline, and a
/// thread parked in a blocking read cannot be cancelled — a task group racing one
/// against a timeout would hang on the way out rather than give up. Here the
/// deadline is just a timer, because nothing is ever blocked.
///
/// `@unchecked Sendable` around a lock: the
/// handler fires on a queue of Foundation's choosing while `tail` is read from the
/// main actor, so every field is behind `lock` and none of them escapes.
final class PipeReader: @unchecked Sendable {
    /// How much of the output is kept for a failure to show. Enough for a config
    /// error with its `anyhow` chain and the lines around it; not so much that a
    /// gateway logging every frame turns this into a memory leak.
    static let tailLines = 60

    private let lock = NSLock()
    /// Bytes seen but not yet split into a line.
    private var pending = Data()
    private var lines: [String] = []
    /// Called once, with the first complete line. `nil` afterwards, which is what
    /// makes "once" true rather than intended.
    private var firstLine: (@Sendable (String) -> Void)?
    /// Called when the pipe closes, which for a child's stdout means it exited
    /// without ever finishing that line.
    private var endOfFile: (@Sendable () -> Void)?
    /// Appended to as output arrives, if this pipe is being logged.
    private let logURL: URL?

    init(
        logURL: URL? = nil,
        firstLine: (@Sendable (String) -> Void)? = nil,
        endOfFile: (@Sendable () -> Void)? = nil
    ) {
        self.logURL = logURL
        self.firstLine = firstLine
        self.endOfFile = endOfFile
    }

    /// Start reading `handle` until it closes.
    func attach(to handle: FileHandle) {
        handle.readabilityHandler = { [self] handle in
            // Empty means the write end is gone: the child has exited or closed it.
            let chunk = handle.availableData
            guard !chunk.isEmpty else {
                handle.readabilityHandler = nil
                finish()
                return
            }
            absorb(chunk)
        }
    }

    /// Stop reading, without waiting for the pipe to close.
    func detach(from handle: FileHandle) {
        handle.readabilityHandler = nil
    }

    /// The last lines seen, oldest first — what a failure panel shows.
    func tail(lines limit: Int = PipeReader.tailLines) -> String {
        lock.withLock { lines.suffix(limit).joined(separator: "\n") }
    }

    private func absorb(_ chunk: Data) {
        // Written from the handler's own queue, which Foundation serializes per
        // handle, so the log keeps the order the gateway wrote in.
        if let logURL {
            Self.append(chunk, to: logURL)
        }
        let (complete, deliver) = lock.withLock { () -> ([String], (@Sendable (String) -> Void)?) in
            pending.append(chunk)
            var complete: [String] = []
            while let newline = pending.firstIndex(of: UInt8(ascii: "\n")) {
                let line = String(decoding: pending[pending.startIndex ..< newline], as: UTF8.self)
                pending.removeSubrange(pending.startIndex ... newline)
                complete.append(line)
            }
            lines.append(contentsOf: complete)
            if lines.count > Self.tailLines * 2 {
                lines.removeFirst(lines.count - Self.tailLines)
            }
            guard !complete.isEmpty, let firstLine else {
                return (complete, nil)
            }
            self.firstLine = nil
            return (complete, firstLine)
        }
        if let deliver, let line = complete.first {
            deliver(line)
        }
    }

    /// The pipe closed. Anything left unterminated is still a line — a gateway that
    /// died mid-message has said the most useful thing it is going to.
    private func finish() {
        let closed = lock.withLock { () -> (@Sendable () -> Void)? in
            if !pending.isEmpty {
                lines.append(String(decoding: pending, as: UTF8.self))
                pending.removeAll()
            }
            let closed = endOfFile
            endOfFile = nil
            firstLine = nil
            return closed
        }
        closed?()
    }

    /// Append to the log, creating it if it is not there.
    ///
    /// Failures are ignored on purpose: this is diagnostic output, and an app that
    /// refused to start a gateway because it could not write a log file would be
    /// trading the whole product for its diary.
    private static func append(_ chunk: Data, to url: URL) {
        if let handle = try? FileHandle(forWritingTo: url) {
            defer { try? handle.close() }
            _ = try? handle.seekToEnd()
            try? handle.write(contentsOf: chunk)
            return
        }
        try? chunk.write(to: url, options: .atomic)
    }
}
