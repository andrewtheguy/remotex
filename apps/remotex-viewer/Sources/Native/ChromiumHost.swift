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
        do {
            try FileManager.default.createDirectory(
                at: profile,
                withIntermediateDirectories: true
            )
        } catch {
            // Said, and then carried on. CEF answers an unusable `cache_path` by
            // falling back to in-memory storage, so the app still runs — it just
            // forgets the client's three remembered preferences at every quit, which
            // is the one failure here with no symptom of its own. Chromium's own
            // complaint names the directory and stops there; this is the half that
            // says why it could not be made.
            FileHandle.standardError.write(Data(
                """
                remotex-viewer: \(profile.path) could not be created, so preferences \
                will not survive a quit: \(error.localizedDescription)

                """.utf8
            ))
        }
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
        ChromiumPump.stop()
        remotex_cef_shutdown()
    }
}

/// CEF asking for a slice of its message pump, from whatever thread it likes.
///
/// A C function pointer can carry no context at all — not even an actor's — so this
/// is a top-level function, and all it does is get onto the main thread.
private func chromiumSchedulePump(
    _ delayMilliseconds: Int64,
    _ context: UnsafeMutableRawPointer?
) {
    DispatchQueue.main.async {
        MainActor.assumeIsolated {
            ChromiumPump.schedule(after: delayMilliseconds)
        }
    }
}

/// Chromium's message loop, run by this app's.
///
/// With `external_message_pump` CEF does not run a loop of its own; it asks, and
/// the host calls `cef_do_message_loop_work`. **Asking is not enough on its own**,
/// and that is the whole reason this type exists rather than a `DispatchQueue.main
/// .asyncAfter`: CEF's request is edge-triggered, so a browser that has gone quiet
/// stops asking, and a load that then needs one more slice never gets it. What that
/// looks like is a window that paints its background and stops — no error, no
/// console message, no failed request, because the renderer is still there waiting
/// for a browser process that has stopped turning. It froze after three slices.
///
/// So the pump keeps a **timer** as well, re-armed after every slice and never
/// longer than 1/30 s. This is cefclient's `MainMessageLoopExternalPump`, port for
/// port: the delay clamp, the reentrancy detection, and running the timer in the
/// event-tracking run-loop mode as well as the common ones — that last one is why
/// Chromium keeps painting while a menu is open, which `DispatchQueue.main` does
/// not do.
@MainActor
enum ChromiumPump {
    /// Never wait longer than this between slices: 1/30 s, as cefclient does.
    private static let maxDelayMilliseconds: Int64 = 1000 / 30

    private static var timer: Timer?
    /// Inside `cef_do_message_loop_work`, which can re-enter through AppKit.
    private static var working = false
    private static var reentered = false

    /// A slice, `delay` from now. Zero runs it at once.
    static func schedule(after delay: Int64) {
        guard ChromiumHost.isRunning else {
            return
        }
        if delay <= 0 {
            work()
        } else {
            setTimer(min(delay, maxDelayMilliseconds))
        }
    }

    /// Take the timer down. Called on the way out, so nothing calls into an engine
    /// that has been shut down.
    static func stop() {
        timer?.invalidate()
        timer = nil
    }

    private static func work() {
        if performWork() {
            // Re-entered: let the stack unwind first, then finish the work.
            DispatchQueue.main.async {
                MainActor.assumeIsolated { ChromiumPump.schedule(after: 0) }
            }
        } else if timer == nil {
            // Nothing pending that we know of — which is exactly when CEF stops
            // asking, so this is the tick that keeps it running.
            setTimer(maxDelayMilliseconds)
        }
    }

    /// One slice. Returns whether it was asked for from inside another.
    private static func performWork() -> Bool {
        if working {
            reentered = true
            return false
        }
        timer?.invalidate()
        timer = nil
        reentered = false
        working = true
        remotex_cef_do_work()
        working = false
        return reentered
    }

    private static func setTimer(_ delay: Int64) {
        timer?.invalidate()
        let timer = Timer(
            timeInterval: max(0.001, Double(delay) / 1000),
            repeats: false
        ) { _ in
            MainActor.assumeIsolated { ChromiumPump.timerFired() }
        }
        // Both modes, and the second is not redundant: a menu being tracked or a
        // window being dragged puts the run loop in `eventTracking`, where a common
        // -mode timer does not fire — and Chromium would stop for as long as the
        // mouse is held down.
        RunLoop.main.add(timer, forMode: .common)
        RunLoop.main.add(timer, forMode: .eventTracking)
        Self.timer = timer
    }

    private static func timerFired() {
        timer = nil
        guard ChromiumHost.isRunning else {
            return
        }
        work()
    }
}
