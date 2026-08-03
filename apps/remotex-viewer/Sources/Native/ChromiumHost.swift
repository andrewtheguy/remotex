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

    /// Take the engine down, then run `quit` — which is the app terminating for
    /// real. Does nothing if a quit is already under way.
    ///
    /// **A quit arrives nested inside `cef_do_message_loop_work`**, and that one fact
    /// is the whole reason this is shaped the way it is. Chromium's pump turns this
    /// app's run loop while it works, so AppKit's own events — the ⌘Q, the signal
    /// handler's `terminate:` — are delivered from *within* a slice. Nothing asked of
    /// CEF on that stack is done: not the close, and not a thousand hand-turned
    /// slices either, which is what quitting used to do before giving up on a
    /// deadline every single time. Waiting there cannot work; `terminateLater` waits
    /// on the same stack, so the slice it is waiting for is the one it is inside.
    ///
    /// So the terminate is *cancelled*, the engine is taken down from a clean stack,
    /// and the app terminates again with nothing left to do. Two passes, and the
    /// second one is the real one.
    static func stopThenQuit(then quit: @escaping @MainActor () -> Void) {
        guard started, !isQuitting else {
            return
        }
        isQuitting = true
        quitWhenDown = quit
        // A browser that will not close must not make the app unquittable. This is
        // the giving-up path, and it leaves the engine standing rather than taking it
        // down out of order — the process is exiting either way. Two seconds is
        // generous: a quit that works takes about a tenth of one.
        deadline = runLoopTimer(after: 2, modes: [.common, .eventTracking, .modalPanel]) {
            MainActor.assumeIsolated { ChromiumHost.abandon() }
        }
        offThePumpStack {
            guard isQuitting else {
                return
            }
            if remotex_cef_close_all(chromiumBrowsersClosed, nil) == 0 {
                // Nothing was open — the launch screen, or a window that never got a
                // page. No callback is coming, so this is the whole of the quit.
                finish()
            }
        }
    }

    /// CEF has closed the last browser. It says so from inside a slice, so the
    /// shutdown that follows has to step off it first.
    fileprivate static func browsersHaveClosed() {
        offThePumpStack { finish() }
    }

    /// Run `work` on the main thread, but never from inside a pump slice.
    ///
    /// The same answer the pump gives its own re-entrancy: let the stack unwind and
    /// look again. It terminates because the slice does — which is true everywhere
    /// except on a stack that is waiting for the quit, and that is exactly the stack
    /// `stopThenQuit` refuses to wait on.
    private static func offThePumpStack(_ work: @escaping @MainActor () -> Void) {
        guard !ChromiumPump.isSlicing else {
            DispatchQueue.main.async {
                MainActor.assumeIsolated { ChromiumHost.offThePumpStack(work) }
            }
            return
        }
        work()
    }

    /// Every browser is closed: take the engine down and terminate for real.
    private static func finish() {
        guard isQuitting else {
            return
        }
        stopWaiting()
        remotex_cef_shutdown()
        quitNow()
    }

    /// Give up waiting and go without shutting Chromium down.
    private static func abandon() {
        guard isQuitting else {
            return
        }
        stopWaiting()
        quitNow()
    }

    private static func stopWaiting() {
        isQuitting = false
        started = false
        deadline?.invalidate()
        deadline = nil
        ChromiumPump.stop()
    }

    private static func quitNow() {
        let quit = quitWhenDown
        quitWhenDown = nil
        quit?()
    }

    /// Set between the close request and the engine being down.
    private static var isQuitting = false
    private static var quitWhenDown: (@MainActor () -> Void)?
    private static var deadline: Timer?
}

/// A one-shot timer that fires in every mode this app waits in.
///
/// `NSModalPanelRunLoopMode` is the one that matters here and the one a plain
/// `Timer.scheduledTimer` would miss: it is the mode AppKit runs while a delegate
/// has answered `terminateLater`, so a timer without it would not fire until the
/// quit it is meant to bound had already finished.
@MainActor
private func runLoopTimer(
    after seconds: TimeInterval,
    modes: [RunLoop.Mode],
    do work: @escaping @Sendable () -> Void
) -> Timer {
    let timer = Timer(timeInterval: seconds, repeats: false) { _ in work() }
    for mode in modes {
        RunLoop.main.add(timer, forMode: mode)
    }
    return timer
}

/// CEF saying the last browser has closed.
///
/// Delivered on its UI thread, which in this process is the main thread — the same
/// assertion `chromiumSchedulePump` makes below and for the same reason.
private func chromiumBrowsersClosed(_ context: UnsafeMutableRawPointer?) {
    MainActor.assumeIsolated {
        ChromiumHost.browsersHaveClosed()
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
    ///
    /// Readable from outside because a quit has to know: what is asked of CEF from
    /// inside a slice is not done. See `closeBrowsersOffThePumpStack`.
    static var isSlicing: Bool { working }
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
        // Three modes, and neither of the last two is redundant. A menu being tracked
        // or a window being dragged puts the run loop in `eventTracking`, where a
        // common-mode timer does not fire — and Chromium would stop for as long as
        // the mouse is held down. `modalPanel` is where AppKit waits out a
        // `terminateLater`, which is where a browser now does its closing: a pump
        // that stopped there would be a quit waiting on work nothing is doing.
        timer = runLoopTimer(
            after: max(0.001, Double(delay) / 1000),
            modes: [.common, .eventTracking, .modalPanel]
        ) {
            MainActor.assumeIsolated { ChromiumPump.timerFired() }
        }
    }

    private static func timerFired() {
        timer = nil
        guard ChromiumHost.isRunning else {
            return
        }
        work()
    }
}
