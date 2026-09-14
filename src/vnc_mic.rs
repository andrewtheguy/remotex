//! The wlshare microphone extension: the browser's microphone, lent to a wlroots-based
//! desktop over the RFB connection the session already has.
//!
//! The camera extension's twin ([`crate::vnc_camera`]), and wlshare's fourth private
//! one. Pseudo-encoding [`ENCODING`] (`WLSM`) and message type [`MSG_MICROPHONE`] are
//! the whole of it, in both directions:
//!
//! - A target with `microphone = true` lists the pseudo-encoding in `SetEncodings`.
//!   wlshare answers with an *available* message; any other server ignores the
//!   encoding and says nothing, and the microphone the browser enables is then never
//!   plugged.
//! - The browser's enable — the mic socket attaching — plugs the microphone, and the
//!   socket going away unplugs it. A plug that arrives before the server's answer
//!   waits for it ([`Device`]).
//! - The server says when an application on the desktop starts recording (*start*,
//!   naming the PCM it wants) and when the last one stops (*stop*); each becomes a
//!   [`MicSignal`] on the bridge, which the mic socket relays to the browser exactly
//!   as it relays an RDP host's.
//! - Samples are the bridge's PCM — the browser's Opus, decoded to the format the
//!   start named — sent while the desktop records and dropped otherwise.
//!
//! The mic socket, the bridge and the browser's encoder are the ones MS-RDPEAI uses
//! ([`crate::mic`]); this is the VNC engine's adapter to them. See
//! docs/wlshare-microphone.md.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use log::{info, warn};
use tokio::sync::{Notify, mpsc};

use crate::mic::{MicBridge, MicControl, MicFormat};
#[cfg(doc)]
use crate::mic::MicSignal;

/// The extension's pseudo-encoding, the ASCII bytes `WLSM`. Listed in `SetEncodings`
/// on a generic target that carries a microphone.
pub const ENCODING: i32 = 0x574c_534d;
/// The extension's one message type, used in both directions. Outside every
/// registered RFB message type.
pub const MSG_MICROPHONE: u8 = 0xE3;

const CLIENT_PLUG: u8 = 0;
const CLIENT_UNPLUG: u8 = 1;
const CLIENT_SAMPLE: u8 = 2;

const SERVER_AVAILABLE: u8 = 0;
const SERVER_START: u8 = 1;
const SERVER_STOP: u8 = 2;

/// Bytes of every server message after its type byte, before a start's format.
pub const SERVER_HEADER_LEN: usize = 3;
/// Bytes of a start's format, after the header.
pub const START_FORMAT_LEN: usize = 8;

/// The most PCM wlshare accepts in one sample. The bridge hands over 20 ms at a
/// time, far below it; a longer run would end the session at wlshare, so it is
/// dropped here instead.
const MAX_SAMPLE: usize = 256 * 1024;

/// How many buffers may wait for the engine's loop to write them: at the bridge's
/// 20 ms, a third of a second. A full queue drops its oldest, so a link that was
/// held up resumes at the live edge.
const SAMPLE_QUEUE: usize = 16;

/// A server message, parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerMicrophone {
    /// The server takes a microphone: the answer to `SetEncodings`.
    Available,
    /// An application is recording, and wants 16-bit PCM in this format.
    Start(MicFormat),
    /// The last application stopped.
    Stop,
}

/// How many bytes follow the header for the operation it names. An operation this
/// client does not know is fatal: the messages share no length field, so one that
/// cannot be measured leaves the stream at an offset nothing recovers from.
pub fn body_len(header: [u8; SERVER_HEADER_LEN]) -> anyhow::Result<usize> {
    match header[0] {
        SERVER_START => Ok(START_FORMAT_LEN),
        SERVER_AVAILABLE | SERVER_STOP => Ok(0),
        other => anyhow::bail!("wlshare microphone operation {other} is not one this client knows"),
    }
}

/// Parse a server message from its header and the [`body_len`] bytes after it.
pub fn parse_server(header: [u8; SERVER_HEADER_LEN], body: &[u8]) -> anyhow::Result<ServerMicrophone> {
    anyhow::ensure!(body.len() == body_len(header)?, "a wlshare microphone message of the wrong length");
    Ok(match header[0] {
        SERVER_AVAILABLE => ServerMicrophone::Available,
        SERVER_START => ServerMicrophone::Start(MicFormat {
            channels: u16::from_be_bytes([body[0], body[1]]),
            // body[2..4]: padding
            sample_rate: u32::from_be_bytes([body[4], body[5], body[6], body[7]]),
        }),
        _ => ServerMicrophone::Stop,
    })
}

/// The plug: `0xE3`, operation 0, two bytes of padding.
fn plug() -> [u8; 4] {
    [MSG_MICROPHONE, CLIENT_PLUG, 0, 0]
}

/// The unplug: `0xE3`, operation 1, two bytes of padding.
fn unplug() -> [u8; 4] {
    [MSG_MICROPHONE, CLIENT_UNPLUG, 0, 0]
}

/// One sample: `0xE3`, operation 2, two bytes of padding, the `u32` length of the
/// PCM, and the PCM.
fn sample(pcm: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8 + pcm.len());
    msg.extend_from_slice(&[MSG_MICROPHONE, CLIENT_SAMPLE, 0, 0]);
    msg.extend_from_slice(&(pcm.len() as u32).to_be_bytes());
    msg.extend_from_slice(pcm);
    msg
}

/// What a plug or an unplug decided: the message to send, if any, and whether a
/// recording the browser was feeding ended with it.
#[derive(Debug, PartialEq, Eq)]
pub struct Decision {
    pub message: Option<Vec<u8>>,
    /// The bridge must hear a close: wlshare sends no stop for a microphone it no
    /// longer has, and the bridge's standing open would otherwise outlive it.
    pub closed: bool,
}

/// Where the microphone stands on one connection: whether the server has said it
/// takes one, whether the browser has plugged one, and whether the desktop records
/// from it. Decided under the uplink, so two decisions reach the wire in the order
/// they were made.
#[derive(Debug, Default)]
pub struct Device {
    announced: bool,
    plugged: bool,
    recording: bool,
}

impl Device {
    /// The server answered `SetEncodings`. It does so on every one, so only the
    /// first is news — and it is what sends a plug the browser made before it.
    pub fn announce(&mut self) -> Option<Vec<u8>> {
        if self.announced {
            return None;
        }
        self.announced = true;
        info!("vnc: the server takes the browser's microphone");
        self.plugged.then(|| plug().to_vec())
    }

    /// The browser enabled its microphone. A microphone already plugged is replaced,
    /// and whatever recorded from the old one is over.
    pub fn plug(&mut self) -> Decision {
        let closed = std::mem::take(&mut self.recording);
        self.plugged = true;
        if !self.announced {
            info!("vnc: holding the microphone's plug until the server says it takes one");
        }
        Decision { message: self.announced.then(|| plug().to_vec()), closed }
    }

    /// The browser disabled its microphone.
    pub fn unplug(&mut self) -> Decision {
        let closed = std::mem::take(&mut self.recording);
        let was = std::mem::take(&mut self.plugged);
        Decision { message: (self.announced && was).then(|| unplug().to_vec()), closed }
    }

    /// The server's start. Heard only for a microphone plugged on the wire: a start
    /// already in flight when the browser unplugged belongs to no microphone.
    pub fn start(&mut self) -> bool {
        self.recording = self.announced && self.plugged;
        self.recording
    }

    /// The server's stop. Returns whether a recording ended.
    pub fn stop(&mut self) -> bool {
        std::mem::take(&mut self.recording)
    }

    /// One buffer of PCM, sent only while the desktop records. Its length was held
    /// to what wlshare accepts before it was queued.
    pub fn sample(&self, pcm: &[u8]) -> Option<Vec<u8>> {
        self.recording.then(|| sample(pcm))
    }
}

/// The microphone's half of one VNC session, shared by the engine's loop and its
/// read loop: the bridge the desktop's decisions go to, and the device.
#[derive(Debug)]
pub struct Link {
    pub bridge: Arc<MicBridge>,
    pub device: Mutex<Device>,
}

/// A command the mic socket gave, as the engine's loop takes it.
#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    Plug,
    Unplug,
    Sample(Vec<u8>),
}

/// The PCM waiting for the engine's loop, newest kept.
#[derive(Default)]
struct Samples {
    buffers: Mutex<VecDeque<Vec<u8>>>,
    ready: Notify,
}

impl Samples {
    fn pop(&self) -> Option<Vec<u8>> {
        self.buffers.lock().expect("microphone sample lock").pop_front()
    }
}

/// The engine loop's end of the mic socket's traffic. Two queues, for the reason the
/// camera has two: a plug or an unplug must never be lost, and late audio is worse
/// than a moment missing.
pub struct Queues {
    commands: mpsc::UnboundedReceiver<Input>,
    samples: Arc<Samples>,
}

impl Queues {
    /// The next thing the socket wants sent, a command ahead of any PCM; `None` once
    /// the control is gone.
    pub async fn next(&mut self) -> Option<Input> {
        loop {
            match self.commands.try_recv() {
                Ok(command) => return Some(command),
                Err(mpsc::error::TryRecvError::Disconnected) => return None,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
            if let Some(pcm) = self.samples.pop() {
                return Some(Input::Sample(pcm));
            }
            tokio::select! {
                biased;
                command = self.commands.recv() => return command,
                // A buffer queued between the check and here left a permit.
                () = self.samples.ready.notified() => {}
            }
        }
    }
}

/// Register the engine's control on the bridge, returning the queues it feeds.
pub fn attach(bridge: &Arc<MicBridge>) -> (Link, Queues) {
    let (commands_tx, commands) = mpsc::unbounded_channel();
    let samples = Arc::new(Samples::default());
    bridge.set_control(Arc::new(Control { commands: commands_tx, samples: Arc::clone(&samples) }));
    (Link { bridge: Arc::clone(bridge), device: Mutex::new(Device::default()) }, Queues { commands, samples })
}

struct Control {
    commands: mpsc::UnboundedSender<Input>,
    samples: Arc<Samples>,
}

impl Control {
    fn clear(&self) {
        self.samples.buffers.lock().expect("microphone sample lock").clear();
    }
}

impl MicControl for Control {
    fn plug(&self) {
        // A closed queue is an engine that has ended, and the device went with it.
        let _ = self.commands.send(Input::Plug);
    }

    /// What was queued for the old microphone goes with it.
    fn unplug(&self) {
        self.clear();
        let _ = self.commands.send(Input::Unplug);
    }

    fn sample(&self, pcm: Vec<u8>) -> bool {
        if pcm.len() > MAX_SAMPLE {
            warn!("vnc: dropped {} bytes of microphone PCM, over the {MAX_SAMPLE} wlshare accepts", pcm.len());
            return false;
        }
        {
            let mut buffers = self.samples.buffers.lock().expect("microphone sample lock");
            if buffers.len() == SAMPLE_QUEUE {
                buffers.pop_front();
            }
            buffers.push_back(pcm);
        }
        self.samples.ready.notify_one();
        true
    }

    /// The recording ended: what it left queued is not the next one's.
    fn reset(&self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONO_48K: MicFormat = MicFormat { channels: 1, sample_rate: 48_000 };

    /// A client message as docs/wlshare-microphone.md lays it out, read back without
    /// the builders.
    #[derive(Debug, PartialEq, Eq)]
    enum Sent {
        Plug,
        Unplug,
        Sample(Vec<u8>),
    }

    fn decode(bytes: &[u8]) -> Sent {
        assert_eq!(bytes[0], 0xE3);
        assert_eq!(&bytes[2..4], &[0, 0], "padding");
        match bytes[1] {
            0 => {
                assert_eq!(bytes.len(), 4);
                Sent::Plug
            }
            1 => {
                assert_eq!(bytes.len(), 4);
                Sent::Unplug
            }
            2 => {
                let len = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
                assert_eq!(bytes.len(), 8 + len, "the message is exactly its PCM");
                Sent::Sample(bytes[8..].to_vec())
            }
            other => panic!("operation {other} is not a client message"),
        }
    }

    #[test]
    fn client_messages_have_the_documented_layouts() {
        assert_eq!(ENCODING, i32::from_be_bytes(*b"WLSM"));
        assert_eq!(plug(), [0xE3, 0, 0, 0]);
        assert_eq!(decode(&plug()), Sent::Plug);
        assert_eq!(decode(&unplug()), Sent::Unplug);
        assert_eq!(sample(&[0x34, 0x12]), [0xE3, 2, 0, 0, 0, 0, 0, 2, 0x34, 0x12]);
        assert_eq!(decode(&sample(&[1, 2, 3, 4])), Sent::Sample(vec![1, 2, 3, 4]));
    }

    #[test]
    fn server_messages_parse_and_an_unknown_one_is_fatal() {
        assert_eq!(body_len([0, 0, 0]).unwrap(), 0);
        assert_eq!(parse_server([0, 0, 0], &[]).unwrap(), ServerMicrophone::Available);
        let format = [0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0xBB, 0x80];
        assert_eq!(body_len([1, 0, 0]).unwrap(), 8);
        assert_eq!(parse_server([1, 0, 0], &format).unwrap(), ServerMicrophone::Start(MONO_48K));
        let stereo = [0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x3E, 0x80];
        assert_eq!(
            parse_server([1, 0, 0], &stereo).unwrap(),
            ServerMicrophone::Start(MicFormat { channels: 2, sample_rate: 16_000 })
        );
        assert_eq!(parse_server([2, 0, 0], &[]).unwrap(), ServerMicrophone::Stop);
        assert!(body_len([3, 0, 0]).is_err());
        assert!(parse_server([1, 0, 0], &format[..4]).is_err());
    }

    /// A plug made before the server answers waits for the answer, and goes out with
    /// it; an answer repeated for a later `SetEncodings` sends nothing again. Samples
    /// go out between a start and a stop alone.
    #[test]
    fn a_plug_before_the_announcement_goes_out_with_it() {
        let mut device = Device::default();
        assert_eq!(device.plug(), Decision { message: None, closed: false });
        assert!(!device.start(), "no start is heard for a plug the server has not seen");
        assert_eq!(device.sample(&[1, 0]), None);
        assert_eq!(device.announce().map(|m| decode(&m)), Some(Sent::Plug));
        assert_eq!(device.announce(), None);
        assert_eq!(device.sample(&[1, 0]), None, "nothing records yet");
        assert!(device.start());
        assert_eq!(decode(&device.sample(&[1, 0]).unwrap()), Sent::Sample(vec![1, 0]));
        assert!(device.stop());
        assert!(!device.stop(), "a stop ends one recording once");
        assert_eq!(device.sample(&[1, 0]), None);
        let unplugged = device.unplug();
        assert_eq!(unplugged.message.map(|m| decode(&m)), Some(Sent::Unplug));
        assert!(!unplugged.closed);
        assert_eq!(device.unplug(), Decision { message: None, closed: false }, "not unplugged twice");
    }

    /// Unplugging or replugging while the desktop records ends the recording at the
    /// bridge — wlshare will not send a stop for a microphone it no longer has — and a
    /// start that was in flight when the browser unplugged is not heard.
    #[test]
    fn unplugging_a_recording_microphone_closes_it() {
        let mut device = Device::default();
        device.announce();
        device.plug();
        assert!(device.start());
        assert_eq!(device.unplug(), Decision { message: Some(unplug().to_vec()), closed: true });
        assert!(!device.start(), "a start for the unplugged microphone");
        assert_eq!(device.sample(&[1, 0]), None);

        assert_eq!(device.plug(), Decision { message: Some(plug().to_vec()), closed: false });
        assert!(device.start());
        assert_eq!(device.plug(), Decision { message: Some(plug().to_vec()), closed: true }, "a replug replaces it");
        assert_eq!(device.sample(&[1, 0]), None, "until the new microphone's start");
    }

    /// A server that never answers — anything but wlshare — is never sent a byte of
    /// the extension, whatever the browser does.
    #[test]
    fn a_server_that_never_answers_hears_nothing() {
        let mut device = Device::default();
        assert_eq!(device.plug().message, None);
        assert_eq!(device.unplug().message, None);
        assert_eq!(device.plug().message, None);
        assert!(!device.start());
        assert_eq!(device.sample(&[1, 0]), None);
    }

    /// Commands go ahead of the PCM queued before them, a full queue keeps the newest
    /// audio, and an unplug or a close drops what was queued.
    #[tokio::test]
    async fn the_queues_keep_commands_first_and_the_newest_audio() {
        let bridge = Arc::new(MicBridge::new());
        let (_link, mut queues) = attach(&bridge);

        bridge.plug();
        let control = Control { commands: mpsc::unbounded_channel().0, samples: Arc::clone(&queues.samples) };
        for n in 0..=SAMPLE_QUEUE {
            assert!(control.sample(vec![n as u8]));
        }
        assert_eq!(queues.next().await, Some(Input::Plug));
        assert_eq!(queues.next().await, Some(Input::Sample(vec![1])), "the oldest was dropped");

        control.reset();
        assert!(queues.samples.pop().is_none(), "a close drops the queue");

        assert!(control.sample(vec![9]));
        bridge.unplug();
        assert_eq!(queues.next().await, Some(Input::Unplug));
        assert!(queues.samples.pop().is_none(), "an unplug drops the queue");

        assert!(!control.sample(vec![0; MAX_SAMPLE + 1]));
        assert!(queues.samples.pop().is_none());
    }

    #[tokio::test]
    async fn a_waiting_loop_wakes_for_audio() {
        let bridge = Arc::new(MicBridge::new());
        let (_link, mut queues) = attach(&bridge);
        let samples = Arc::clone(&queues.samples);
        let waiting = tokio::spawn(async move { queues.next().await });
        tokio::task::yield_now().await;
        let control = Control { commands: mpsc::unbounded_channel().0, samples };
        assert!(control.sample(vec![7, 0]));
        assert_eq!(waiting.await.unwrap(), Some(Input::Sample(vec![7, 0])));
    }
}
