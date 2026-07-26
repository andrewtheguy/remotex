//! The MS-RDPECLIP half of the clipboard bridge: a [`CliprdrBackend`] that does
//! nothing but forward, plus the text conversions either direction needs.
//!
//! ## Why the backend is inert
//!
//! IronRDP calls [`CliprdrBackend`] synchronously from inside
//! `ActiveStage::process`, and the methods cannot return PDUs — a backend that
//! wants to answer has to call back into `Cliprdr` afterwards, which it does not
//! own. So every callback here just drops a [`ClipboardEvent`] into a channel
//! and `src/rdp.rs` acts on it from the session loop, where the `ActiveStage`
//! actually lives. That also keeps all the clipboard *state* in one task
//! instead of behind a mutex shared with a channel processor.
//!
//! ## Delayed rendering
//!
//! Unlike VNC's `ServerCutText` (which carries the text) and the Mac agent's
//! pasteboard read, RDP only announces *which formats* the remote clipboard now
//! holds; the bytes cost a second round trip. Both directions are lazy on the
//! wire:
//!
//! - remote copies → `FormatList` → we ask with `FormatDataRequest`;
//! - browser copies → we advertise `FormatList` → the remote asks when the user
//!   actually pastes, and only then do we hand over the text.
//!
//! The gateway hides that from the browser by asking immediately, so a copy on
//! the remote reaches the browser unprompted exactly as it does for the other
//! two engines.
//!
//! ## Scope
//!
//! `CF_UNICODETEXT` only. The browser protocol carries a `text` string, so
//! HTML, bitmaps and file lists have nowhere to go — see docs/roadmap.md.

use ironrdp::cliprdr::backend::CliprdrBackend;
use ironrdp::cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp::core::impl_as_any;
use log::{debug, warn};
use tokio::sync::mpsc;

use crate::protocol::clipboard_fits;

/// What the channel processor noticed, for the session loop to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardEvent {
    /// Capability exchange finished; the channel can carry data now.
    Ready,
    /// The channel wants our clipboard advertised. Load-bearing during startup:
    /// the first `initiate_copy` is what carries the Capabilities and
    /// TemporaryDirectory PDUs, so ignoring this stalls the handshake.
    FormatListRequested,
    /// The remote's clipboard changed and now holds these formats.
    RemoteFormats(Vec<ClipboardFormat>),
    /// Decoded text returned for the format we requested.
    RemoteData(String),
    /// The remote returned CB_RESPONSE_FAIL. This can be retried.
    RemoteDataRefused,
    /// The remote returned data that was not valid Unicode clipboard text.
    RemoteDataMalformed,
    /// The remote is pasting and wants our text in this format.
    DataRequested(ClipboardFormatId),
}

/// The [`CliprdrBackend`] IronRDP drives. Holds no clipboard state of its own.
#[derive(Debug)]
pub struct Backend {
    tx: mpsc::UnboundedSender<ClipboardEvent>,
}

impl_as_any!(Backend);

impl Backend {
    pub fn new(tx: mpsc::UnboundedSender<ClipboardEvent>) -> Self {
        Self { tx }
    }

    /// A closed channel means the session loop is gone and this connection is
    /// being torn down, so it is not worth more than a debug line.
    fn emit(&self, event: ClipboardEvent) {
        if self.tx.send(event).is_err() {
            debug!("rdp: clipboard event dropped, the session loop has ended");
        }
    }
}

impl CliprdrBackend for Backend {
    /// Never used: file transfer is not negotiated (see
    /// [`Self::client_capabilities`]), so nothing is ever written to disk.
    fn temporary_directory(&self) -> &str {
        "."
    }

    /// Deliberately empty. Every flag here is about file transfer — streamed
    /// file clips, long format names for them, and the clipboard locking that
    /// keeps a file list alive across a copy. Advertising none of it keeps the
    /// remote from ever sending a file-contents request.
    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::empty()
    }

    fn on_ready(&mut self) {
        self.emit(ClipboardEvent::Ready);
    }

    fn on_request_format_list(&mut self) {
        self.emit(ClipboardEvent::FormatListRequested);
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        capabilities: ClipboardGeneralCapabilityFlags,
    ) {
        debug!("rdp: clipboard capabilities negotiated: {capabilities:?}");
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        self.emit(ClipboardEvent::RemoteFormats(available_formats.to_vec()));
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        self.emit(ClipboardEvent::DataRequested(request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let event = if response.is_error() {
            // CB_RESPONSE_FAIL says only that the format-data request was not
            // processed successfully; the wire response carries no cause.
            debug!("rdp: the remote failed the clipboard format-data request");
            ClipboardEvent::RemoteDataRefused
        } else {
            match response.to_unicode_string() {
                Ok(text) => ClipboardEvent::RemoteData(text),
                Err(e) => {
                    warn!("rdp: undecodable clipboard text from the remote: {e}");
                    ClipboardEvent::RemoteDataMalformed
                }
            }
        };
        self.emit(event);
    }

    /// Unreachable: [`Self::client_capabilities`] advertises no file support,
    /// so the remote has no reason to ask. Logged rather than ignored, because
    /// arriving here means the remote disregarded the negotiated capabilities.
    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {
        warn!("rdp: ignoring a clipboard file-contents request; file transfer is not supported");
    }

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {
        warn!("rdp: ignoring a clipboard file-contents response; file transfer is not supported");
    }

    // Locking exists to hold a file list still while it is being read. With no
    // file transfer there is nothing to hold, so both are no-ops.
    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

/// The one format worth asking for out of what the remote advertised.
///
/// `CF_UNICODETEXT` or nothing. There is deliberately no `CF_TEXT` fallback:
/// that format is ANSI in the remote's code page, which we would have to guess
/// at, and a server offering text at all offers the Unicode flavour beside it.
/// An image or file-list copy simply produces no browser-visible clipboard.
pub fn pick_text_format(formats: &[ClipboardFormat]) -> Option<ClipboardFormatId> {
    formats
        .iter()
        .map(ClipboardFormat::id)
        .find(|&id| id == ClipboardFormatId::CF_UNICODETEXT)
}

/// Remote → browser: CRLF to LF.
///
/// Windows clipboard text uses CRLF. Passing it through would put a stray `\r`
/// at the end of every line in the browser's textarea and in the local OS
/// clipboard.
///
/// `Err(len)` when the text exceeds [`MAX_CLIPBOARD_BYTES`](crate::protocol::MAX_CLIPBOARD_BYTES), carrying the size
/// it was: the caller reports that instead, because a truncated paste is
/// indistinguishable from a whole one.
pub fn from_remote(text: &str) -> Result<String, u64> {
    // Some servers pad the response past the terminator IronRDP already
    // stripped; a trailing NUL renders as a replacement glyph in the panel.
    let text = text.trim_end_matches('\0');
    if !clipboard_fits(text) {
        return Err(text.len() as u64);
    }
    Ok(text.replace("\r\n", "\n"))
}

/// Browser → remote: LF to CRLF.
///
/// Normalizing to LF before expanding is what keeps text that already contains
/// CRLF from coming out with doubled line breaks.
///
/// `None` when the text exceeds [`MAX_CLIPBOARD_BYTES`](crate::protocol::MAX_CLIPBOARD_BYTES) — the remote keeps what
/// it had rather than taking ownership of a partial copy. The expansion can
/// still overshoot the ceiling by the number of line breaks, which is the
/// remote's own clipboard and no longer this link's problem.
pub fn to_remote(text: &str) -> Option<String> {
    clipboard_fits(text).then(|| text.replace("\r\n", "\n").replace('\n', "\r\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MAX_CLIPBOARD_BYTES;

    #[test]
    fn unix_line_endings_become_windows_ones() {
        assert_eq!(to_remote("one\ntwo").unwrap(), "one\r\ntwo");
        assert_eq!(to_remote("trailing\n").unwrap(), "trailing\r\n");
    }

    // The bug this guards: normalizing first is what stops "\r\n" turning into
    // "\r\r\n" and doubling every line break on the remote.
    #[test]
    fn windows_line_endings_are_not_doubled() {
        assert_eq!(to_remote("one\r\ntwo").unwrap(), "one\r\ntwo");
        assert_eq!(
            to_remote("mixed\r\nand\nmatched").unwrap(),
            "mixed\r\nand\r\nmatched"
        );
    }

    // A lone CR is not a line ending in either convention, so neither
    // direction may invent one around it.
    #[test]
    fn a_bare_carriage_return_survives_both_directions() {
        assert_eq!(to_remote("a\rb").unwrap(), "a\rb");
        assert_eq!(from_remote("a\rb").unwrap(), "a\rb");
    }

    #[test]
    fn windows_line_endings_come_back_as_unix_ones() {
        assert_eq!(from_remote("one\r\ntwo").unwrap(), "one\ntwo");
        assert_eq!(from_remote("no line breaks").unwrap(), "no line breaks");
    }

    #[test]
    fn a_round_trip_through_the_remote_preserves_the_text() {
        for original in ["", "plain", "one\ntwo\nthree", "画面 ☕\nemoji 🚀"] {
            assert_eq!(
                from_remote(&to_remote(original).unwrap()).unwrap(),
                original,
                "{original:?}"
            );
        }
    }

    // Padding past the string terminator would otherwise reach the browser as
    // a replacement glyph on the end of every paste.
    #[test]
    fn trailing_nul_padding_is_stripped() {
        assert_eq!(from_remote("text\0").unwrap(), "text");
        assert_eq!(from_remote("text\0\0\0").unwrap(), "text");
        // Only trailing: a NUL in the middle is the remote's own data.
        assert_eq!(from_remote("a\0b").unwrap(), "a\0b");
    }

    // Refused in both directions rather than truncated: a partial paste is
    // indistinguishable from a whole one, on either side of the link.
    #[test]
    fn oversized_text_is_refused_in_both_directions() {
        let text = "é".repeat(MAX_CLIPBOARD_BYTES); // two bytes each, so 2x over
        assert_eq!(to_remote(&text), None);
        assert_eq!(from_remote(&text), Err(text.len() as u64));

        // At the ceiling both still pass, so the boundary is inclusive.
        let fits = "a".repeat(MAX_CLIPBOARD_BYTES);
        assert_eq!(to_remote(&fits).unwrap().len(), MAX_CLIPBOARD_BYTES);
        assert_eq!(from_remote(&fits).unwrap().len(), MAX_CLIPBOARD_BYTES);

        // The size reported is the text's own, measured after the NUL padding
        // this direction strips.
        let padded = format!("{}\0\0", "a".repeat(MAX_CLIPBOARD_BYTES + 5));
        assert_eq!(from_remote(&padded), Err(MAX_CLIPBOARD_BYTES as u64 + 5));
    }

    #[test]
    fn unicode_text_is_the_only_format_taken() {
        let unicode = ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT);
        let ansi = ClipboardFormat::new(ClipboardFormatId::CF_TEXT);
        let bitmap = ClipboardFormat::new(ClipboardFormatId::CF_BITMAP);

        assert_eq!(
            pick_text_format(&[ansi.clone(), unicode.clone(), bitmap.clone()]),
            Some(ClipboardFormatId::CF_UNICODETEXT)
        );
        assert_eq!(
            pick_text_format(&[unicode]),
            Some(ClipboardFormatId::CF_UNICODETEXT)
        );

        // ANSI alone is refused rather than guessed at: CF_TEXT is in the
        // remote's code page, which nothing here knows.
        assert_eq!(pick_text_format(&[ansi]), None);
        assert_eq!(pick_text_format(&[bitmap]), None);
        assert_eq!(pick_text_format(&[]), None);
    }

    // The UTF-16LE encoding and its NUL terminator belong to IronRDP, not to
    // this module. This pins that assumption: if the library ever stops
    // round-tripping, the engine would silently ship mojibake.
    #[test]
    fn ironrdp_owns_the_utf16_encoding() {
        for original in ["plain", "画面 ☕", "emoji 🚀 non-BMP", "", "line\r\nbreak"] {
            let response = FormatDataResponse::new_unicode_string(original);
            assert!(!response.is_error());
            assert_eq!(response.to_unicode_string().unwrap(), original, "{original:?}");
        }
    }

    #[test]
    fn the_backend_forwards_every_callback_it_is_given() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut backend = Backend::new(tx);

        backend.on_ready();
        backend.on_request_format_list();
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]);
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });
        backend.on_format_data_response(FormatDataResponse::new_unicode_string("画面"));
        backend.on_format_data_response(FormatDataResponse::new_error());
        backend.on_format_data_response(FormatDataResponse::new_data(vec![0x00, 0xd8]));

        assert_eq!(rx.try_recv().unwrap(), ClipboardEvent::Ready);
        assert_eq!(rx.try_recv().unwrap(), ClipboardEvent::FormatListRequested);
        assert_eq!(
            rx.try_recv().unwrap(),
            ClipboardEvent::RemoteFormats(vec![ClipboardFormat::new(
                ClipboardFormatId::CF_UNICODETEXT
            )])
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            ClipboardEvent::DataRequested(ClipboardFormatId::CF_UNICODETEXT)
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            ClipboardEvent::RemoteData("画面".to_owned())
        );
        assert_eq!(rx.try_recv().unwrap(), ClipboardEvent::RemoteDataRefused);
        assert_eq!(rx.try_recv().unwrap(), ClipboardEvent::RemoteDataMalformed);
        assert!(rx.try_recv().is_err(), "no extra events");
    }

    // A backend whose session loop has gone must not panic — the connection is
    // being torn down and IronRDP may still drive a callback or two.
    #[test]
    fn a_dead_session_loop_does_not_panic_the_backend() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        Backend::new(tx).on_ready();
    }
}
