//! One whole frame off the socket, of whichever framing it happens to wear.
//!
//! A live RDP connection carries two: the TPKT frames of [`super::x224`], which
//! everything the client sends and everything but an update travels in, and the
//! fast-path frames of [`super::fastpath`], which the server's updates use because
//! they cost four bytes fewer. They are told apart by the first byte and share a
//! socket, so reading one means reading whichever arrives.
//!
//! Neither framing is self-delimiting on the socket: a read returns whatever the
//! kernel had, which is half a frame as readily as three. [`Frames`] is the buffer
//! that turns that back into PDUs.
//!
//! # Cancel safety
//!
//! [`Frames::next`] is meant to be one arm of a `select!` beside a queue of things to
//! send, so it has to survive being dropped part-way through a frame. Every byte read
//! stays in the buffer until a whole frame is there, and the only await inside is a
//! read that consumes nothing when it is cancelled — so a caller that gives up on a
//! half-arrived frame loses nothing but the wait.

use anyhow::{Context as _, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use super::wire::Malformed;
use super::{fastpath, x224};

/// How much room to have free before asking the socket for more. Large enough that a
/// full-screen bitmap update is a handful of reads rather than a hundred.
const READ: usize = 64 * 1024;

/// The frames arriving on one connection.
pub struct Frames<S> {
    stream: S,
    /// What has arrived and not yet been handed out: at most one whole frame and
    /// whatever came after it in the same read.
    buffer: Vec<u8>,
}

impl<S: AsyncRead + Unpin> Frames<S> {
    pub fn new(stream: S) -> Self {
        Self { stream, buffer: Vec::with_capacity(READ) }
    }

    /// Read one whole frame into `out`, which is cleared first.
    ///
    /// The frame is copied out rather than borrowed so that the caller can go on to
    /// use the connection — a frame is answered as often as not — while it reads it.
    pub async fn next(&mut self, out: &mut Vec<u8>) -> Result<()> {
        loop {
            if let Some(length) = self.length()?
                && self.buffer.len() >= length
            {
                out.clear();
                out.extend_from_slice(&self.buffer[..length]);
                self.buffer.drain(..length);
                return Ok(());
            }
            self.buffer.reserve(READ);
            let read = self
                .stream
                .read_buf(&mut self.buffer)
                .await
                .context("reading from the host")?;
            if read == 0 {
                bail!("the host closed the connection");
            }
        }
    }

    /// How long the frame at the front of the buffer is, or `None` while too little
    /// of its header has arrived to say.
    fn length(&self) -> Result<Option<usize>, Malformed> {
        let Some(&first) = self.buffer.first() else {
            return Ok(None);
        };
        if fastpath::is_output(first) {
            return fastpath::frame_length(&self.buffer);
        }
        let Some(header) = self.buffer.get(..x224::TPKT_HEADER) else {
            return Ok(None);
        };
        let header: &[u8; x224::TPKT_HEADER] = header.try_into().expect("exactly the header");
        x224::frame_length(header).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TPKT frame of `length` bytes, and a fast-path one.
    fn tpkt(length: u16) -> Vec<u8> {
        let mut frame = vec![0x03, 0x00];
        frame.extend_from_slice(&length.to_be_bytes());
        frame.resize(usize::from(length), 0xAA);
        frame
    }

    fn fast(length: u8) -> Vec<u8> {
        let mut frame = vec![0x00, length];
        frame.resize(usize::from(length), 0xBB);
        frame
    }

    async fn collect(bytes: &[u8], frames: usize) -> Vec<Vec<u8>> {
        let mut reader = Frames::new(bytes);
        let mut out = Vec::new();
        let mut frame = Vec::new();
        for _ in 0..frames {
            reader.next(&mut frame).await.expect("a frame");
            out.push(frame.clone());
        }
        out
    }

    #[tokio::test]
    async fn the_two_framings_share_a_socket_and_are_told_apart_by_their_first_byte() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&tpkt(9));
        wire.extend_from_slice(&fast(7));
        wire.extend_from_slice(&tpkt(300));
        let frames = collect(&wire, 3).await;
        assert_eq!(frames[0], tpkt(9));
        assert_eq!(frames[1], fast(7));
        assert_eq!(frames[2], tpkt(300));
    }

    /// The point of the buffer: what the kernel hands over has nothing to do with
    /// where frames end.
    #[tokio::test]
    async fn a_frame_split_across_reads_arrives_whole() {
        let whole = tpkt(64);
        let (first, second) = whole.split_at(3);
        // `chain` empties the first reader before touching the second, so the frame
        // needs two reads to arrive whatever the buffer's size.
        let mut reader = Frames::new(first.chain(second));
        let mut frame = Vec::new();
        reader.next(&mut frame).await.expect("a frame");
        assert_eq!(frame, whole);
    }

    #[tokio::test]
    async fn a_connection_that_ends_mid_frame_says_so_rather_than_hanging() {
        let cut = tpkt(64);
        let mut reader = Frames::new(&cut[..10]);
        let mut frame = Vec::new();
        let err = reader.next(&mut frame).await.unwrap_err();
        assert_eq!(err.to_string(), "the host closed the connection");
    }

    /// A header this client cannot read is an error rather than a wait for bytes
    /// that would make it make sense.
    #[tokio::test]
    async fn a_frame_that_is_neither_framing_is_refused() {
        let mut reader = Frames::new(&[0x03, 0x00, 0x00, 0x01][..]);
        let mut frame = Vec::new();
        assert!(reader.next(&mut frame).await.is_err());
    }
}
