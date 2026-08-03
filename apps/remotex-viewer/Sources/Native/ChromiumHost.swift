import AppKit
import CRemotexCEF
import Foundation

/// Chromium, for as long as this process runs.
///
/// One of these, brought up before the window and taken down on the way out. It is
/// separate from `NativeBridge` because their lifetimes are not the same: the
/// engine is the process's, while a bridge is one surface's and comes and goes with
/// every target switch.
@MainActor
enum ChromiumHost {
    private static var started = false

    /// Bring the engine up, showing the SPA in `webRoot` and keeping this
    /// instance's profile in `profile`.
    ///
    /// Returns false when Chromium refused to start, which is terminal — there is
    /// no second engine to fall back to, and the app says so rather than opening an
    /// empty window.
    @discardableResult
    static func start(webRoot: URL, profile: URL) -> Bool {
        guard !started else {
            return true
        }
        try? FileManager.default.createDirectory(
            at: profile,
            withIntermediateDirectories: true
        )
        started = webRoot.path.withCString { webRoot in
            profile.path.withCString { profile in
                // A plain function reference rather than a closure: a C function
                // pointer can capture nothing, and anything main-actor-isolated
                // counts as captured context.
                remotex_cef_initialize(webRoot, profile, chromiumSchedulePump, nil)
            }
        }
        return started
    }

    /// Whether the engine is up. Read from the pump, which arrives from outside the
    /// actor and must not run `do_message_loop_work` after `stop`.
    static var isRunning: Bool {
        started
    }

    /// Take the engine down. Synchronous, because the process may be gone the
    /// moment this returns.
    static func stop() {
        guard started else {
            return
        }
        started = false
        remotex_cef_shutdown()
    }
}

/// Run one slice of Chromium's message pump, `delayMilliseconds` from now.
///
/// Always asynchronously, and never straight from the callback that asked for it:
/// CEF schedules this from inside its own pump, so calling back in on that stack
/// re-enters it. Hopping through the main queue is what keeps the two loops —
/// AppKit's and Chromium's — taking turns rather than nesting.
///
/// A file-private top-level function because this becomes a C function pointer, and
/// one of those can carry no context at all — not even an actor's.
private func chromiumSchedulePump(
    _ delayMilliseconds: Int64,
    _ context: UnsafeMutableRawPointer?
) {
    let delay = Int(max(0, delayMilliseconds))
    DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(delay)) {
        MainActor.assumeIsolated {
            // A pump scheduled before `stop` can still be in the queue after it,
            // and running it then is a call into a shut-down engine.
            guard ChromiumHost.isRunning else {
                return
            }
            remotex_cef_do_work()
        }
    }
}
