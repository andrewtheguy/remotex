import Foundation

/// The surface `AppModel` draws on, which is everything it knows about the web
/// view underneath.
///
/// Two methods, because the model's whole part in rendering is forwarding: it
/// decides *what* the page is told (a `resize` before the tiles in its space, a
/// `clear` when a session ends) and never how anything is drawn. That was the
/// point of the change this protocol came out of — the Metal renderer it
/// replaced had a texture, a shader, a tile decoder and a scroll view behind it,
/// all of which the model had to be arranged around.
///
/// `AnyObject` so the model can hold it weakly: the canvas belongs to the view
/// that is on screen, and the model outlives it.
protocol RemoteCanvas: AnyObject, Sendable {
    /// Queue one control command.
    func send(_ command: CanvasCommand)
    /// Queue one gateway binary frame, verbatim — the page parses it with the
    /// same code the browser SPA uses, so nothing here reads or rewrites it.
    func send(frame: Data)
}

extension CanvasServer: RemoteCanvas {}
