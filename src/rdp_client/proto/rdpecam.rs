//! Video capture redirection (MS-RDPECAM): a camera on this end, as a device on the host.
//!
//! Two kinds of dynamic channel carry it, both opened by the host. The **device
//! enumeration channel**, [`ENUMERATOR`], is where the two ends agree a version — this
//! end asks for [`VERSION`], and the host answers with the highest it speaks that is
//! not higher — and where this end announces a device, by a display name and the name
//! of a second channel. The host then opens that **device channel**, [`DEVICE_CHANNEL`],
//! and everything about the device happens there: the host asks, this end answers, one
//! request one response, on the channel the request came in on.
//!
//! What the host asks is a camera's whole life. Activate it; list its streams and each
//! stream's formats; start a stream in one of those formats; ask for samples; stop;
//! deactivate. [MS-RDPECAM] 3.1.1 gives the device three states — Deactivated,
//! Activated, Streaming — and says which requests each takes, and [`Rdpecam`] keeps
//! them: a request the state does not take is answered with the error the specification
//! names for it rather than acted on, and a malformed or unexpected one with
//! `InvalidMessage`, as 3.2.5 requires. On the enumeration channel, where no error
//! response exists, such a message is discarded (3.1.5). Nothing a host sends on either
//! channel ends the session.
//!
//! # One device, several channels
//!
//! A host may open the device channel more than once, under the one name, and keep
//! every instance open: measured against a Windows Enterprise host, the Device
//! Initialization sequence runs on one instance and, before that instance is closed or
//! deactivated, a second is opened for the Device Control Initialization sequence. The
//! device is still one device, so its state is kept once, as 3.1.1 describes it — the
//! Activate requests on every instance nest into one count, and a Deactivate on any of
//! them ends a stream, as FreeRDP's client has it. Samples go out on the instance that
//! started the stream or last asked for one, and an instance that closes takes back the
//! activations it made and, if the stream was its, the stream.
//!
//! # The one stream
//!
//! One stream, color, in one format: H.264 at the geometry and rate the caller plugged
//! the device with, which is what the caller's encoder produces. The host chooses from a
//! list of one, and 3.1.1 has it choose from the list, so a Start Streams naming
//! anything else is refused as `InvalidMediaType`. A sample is one H.264 picture with
//! its start codes and parameter sets inline (2.2.3.8.1); this module never looks inside
//! one.
//!
//! # Samples are metered
//!
//! The host asks for each sample with a Sample Request, and each is owed exactly one
//! Sample Response or Sample Error Response (2.2.3.13). A sample the caller hands over
//! while nothing is owed waits, at most [`PENDING`] deep; past that the queue is dropped
//! whole and nothing but a keyframe is taken until one arrives, because a decoder handed
//! the far side of a gap in H.264 shows the gap until the next keyframe anyway. The
//! caller is told once per gap, with [`Output::KeyframeNeeded`].
//!
//! # Version 2
//!
//! Version 2 adds device properties — brightness, focus and the like — and every version
//! below the highest a client claims must be supported too (1.3.1). This device has no
//! properties: under version 2 the list is empty and a question about any one of them
//! is `ItemNotFound`, and under version 1 those requests do not exist and are
//! `InvalidMessage`.
//!
//! [MS-RDPECAM]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpecam/92af6790-b79c-4813-9c07-7c545bed0242

use std::collections::VecDeque;

use log::{debug, warn};

use super::wire::Writer;

/// The device enumeration channel's name, fixed by [MS-RDPECAM] 2.1.
pub const ENUMERATOR: &str = "RDCamera_Device_Enumerator";

/// The device channel's name. The protocol leaves it to the client, which names it in the
/// Device Added Notification; one device is all this end announces.
pub const DEVICE_CHANNEL: &str = "Remotex_Camera_0";

/// The highest version this end speaks, and so the one it asks for.
pub const VERSION: u8 = 2;

/// How many samples may wait for the host's Sample Requests before the queue is dropped.
///
/// A host asks for samples ahead of time, so the queue is short-lived when the link
/// keeps up; one deeper than a few frames is a camera falling behind the link, and
/// waiting longer only makes the picture later.
pub const PENDING: usize = 8;

// SHARED_MSG_HEADER's MessageId, 2.2.1.
const SUCCESS_RESPONSE: u8 = 0x01;
const ERROR_RESPONSE: u8 = 0x02;
const SELECT_VERSION_REQUEST: u8 = 0x03;
const SELECT_VERSION_RESPONSE: u8 = 0x04;
const DEVICE_ADDED_NOTIFICATION: u8 = 0x05;
const DEVICE_REMOVED_NOTIFICATION: u8 = 0x06;
const ACTIVATE_DEVICE_REQUEST: u8 = 0x07;
const DEACTIVATE_DEVICE_REQUEST: u8 = 0x08;
const STREAM_LIST_REQUEST: u8 = 0x09;
const STREAM_LIST_RESPONSE: u8 = 0x0A;
const MEDIA_TYPE_LIST_REQUEST: u8 = 0x0B;
const MEDIA_TYPE_LIST_RESPONSE: u8 = 0x0C;
const CURRENT_MEDIA_TYPE_REQUEST: u8 = 0x0D;
const CURRENT_MEDIA_TYPE_RESPONSE: u8 = 0x0E;
const START_STREAMS_REQUEST: u8 = 0x0F;
const STOP_STREAMS_REQUEST: u8 = 0x10;
const SAMPLE_REQUEST: u8 = 0x11;
const SAMPLE_RESPONSE: u8 = 0x12;
const SAMPLE_ERROR_RESPONSE: u8 = 0x13;
const PROPERTY_LIST_REQUEST: u8 = 0x14;
const PROPERTY_LIST_RESPONSE: u8 = 0x15;
const PROPERTY_VALUE_REQUEST: u8 = 0x16;
const PROPERTY_VALUE_RESPONSE: u8 = 0x17;
const SET_PROPERTY_VALUE_REQUEST: u8 = 0x18;

// ErrorCode, 2.2.3.2 — the ones this end has cause to send.
const UNEXPECTED_ERROR: u32 = 0x01;
const INVALID_MESSAGE: u32 = 0x02;
const NOT_INITIALIZED: u32 = 0x03;
const INVALID_REQUEST: u32 = 0x04;
const INVALID_STREAM_NUMBER: u32 = 0x05;
const INVALID_MEDIA_TYPE: u32 = 0x06;
const ITEM_NOT_FOUND: u32 = 0x08;
const SET_NOT_FOUND: u32 = 0x09;

/// STREAM_DESCRIPTION, 2.2.3.6.1: color frames, capture category.
const FRAME_SOURCE_COLOR: u16 = 0x0001;
const STREAM_CATEGORY_CAPTURE: u8 = 0x01;

/// MEDIA_TYPE_DESCRIPTION, 2.2.3.8.1: H.264, which the host decodes.
const FORMAT_H264: u8 = 0x01;
const DECODING_REQUIRED: u8 = 0x01;
const MEDIA_TYPE_BYTES: usize = 26;
/// START_STREAM_INFO, 2.2.3.11.1: a stream index and a media type.
const START_STREAM_INFO_BYTES: usize = 1 + MEDIA_TYPE_BYTES;

/// PROPERTY_DESCRIPTION's PropertySet (2.2.3.17.1) and PROPERTY_VALUE's Mode (2.2.3.19.1).
const PROPERTY_SET_CAMERA_CONTROL: u8 = 0x01;
const PROPERTY_SET_VIDEO_PROC_AMP: u8 = 0x02;
const PROPERTY_MODE_MANUAL: u8 = 0x01;
const PROPERTY_MODE_AUTO: u8 = 0x02;

/// The longest device channel name 2.1 allows, before its terminator.
const MAX_CHANNEL_NAME: usize = 256;

/// The geometry and rate of the H.264 a device produces: its one media type.
///
/// The rate is a ratio because the wire's is: 29.97 frames a second is 30000/1001.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    pub fps_numerator: u32,
    pub fps_denominator: u32,
}

impl Format {
    /// Whether this is a media type at all: a picture with an area, and a rate with
    /// neither half zero.
    pub fn is_describable(self) -> bool {
        self.width > 0 && self.height > 0 && self.fps_numerator > 0 && self.fps_denominator > 0
    }

    /// The MEDIA_TYPE_DESCRIPTION: square pixels, and a format the host decodes.
    fn media_type(self) -> [u8; MEDIA_TYPE_BYTES] {
        let mut w = Writer::with_capacity(MEDIA_TYPE_BYTES);
        w.u8(FORMAT_H264);
        w.u32_le(self.width);
        w.u32_le(self.height);
        w.u32_le(self.fps_numerator);
        w.u32_le(self.fps_denominator);
        w.u32_le(1); // PixelAspectRatioNumerator
        w.u32_le(1); // PixelAspectRatioDenominator
        w.u8(DECODING_REQUIRED);
        w.finish().try_into().expect("a media type is written to its exact size")
    }
}

/// What a turn amounted to, beyond the messages it put on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// The host agreed a version on the enumeration channel: redirection is on offer.
    Negotiated { version: u8 },
    /// The host answered with a version this end does not speak. The protocol stops
    /// there (3.2.5.2), and nothing is announced on that channel.
    VersionRefused { version: u8 },
    /// The host opened a channel for the device it was told about, where it had none
    /// open before.
    Attached,
    /// The host started the stream in this format: samples are wanted, from a keyframe.
    Started(Format),
    /// The host stopped the stream — Stop Streams, Deactivate Device, or the stream's
    /// channel closing. The device stays announced, and another start may follow.
    Stopped,
    /// Samples were dropped, and the next one taken must be a keyframe.
    KeyframeNeeded,
}

/// One event acted on: what to send, and what it meant.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Turn {
    /// Whole messages, each for the dynamic channel numbered beside it, in order.
    pub replies: Vec<(u32, Vec<u8>)>,
    pub outputs: Vec<Output>,
}

impl Turn {
    fn reply(&mut self, channel: u32, message: Vec<u8>) {
        self.replies.push((channel, message));
    }
}

/// One open instance of the device channel.
#[derive(Debug)]
struct Instance {
    channel: u32,
    /// The Activate Device Requests made on this instance that no Deactivate has matched,
    /// which the device's count gives back when the instance closes.
    activations: u32,
}

/// The client's side of camera redirection, over all of its channels.
#[derive(Debug)]
pub struct Rdpecam {
    /// The display name the host shows beside the device.
    name: String,
    /// The version every message after the negotiation carries: [`VERSION`] until the
    /// host has answered, and the host's answer after.
    version: u8,
    /// The enumeration channel, while the host holds it open.
    enumerator: Option<u32>,
    /// Whether the host answered the version request with a version this end speaks.
    negotiated: bool,
    /// Whether it answered with one this end does not, which ends the protocol on that
    /// channel.
    refused: bool,
    /// The device's format, while the caller has it plugged.
    format: Option<Format>,
    /// Whether a Device Added Notification for it has gone out on the open enumeration
    /// channel.
    announced: bool,
    /// The device channel's open instances, for the device as it is plugged now.
    devices: Vec<Instance>,
    /// Instances still open for a device since withdrawn, which answer nothing but
    /// `InvalidRequest` until the host closes them.
    withdrawn: Vec<u32>,
    /// Activate Device Requests not yet matched by a Deactivate, over every instance: the
    /// device is Deactivated at zero (3.1.1).
    activations: u32,
    /// Whether the device is Streaming.
    streaming: bool,
    /// Where samples go: the instance that started the stream or last asked for a sample.
    stream_channel: Option<u32>,
    /// Sample Requests not yet answered.
    credits: u32,
    /// Samples waiting for a Sample Request, oldest first.
    pending: VecDeque<Vec<u8>>,
    /// Whether samples are dropped until a keyframe: at the start of every stream, and
    /// after a gap.
    awaiting_keyframe: bool,
    /// Whether the caller has been told about the current gap.
    keyframe_asked: bool,
}

impl Rdpecam {
    /// A device the host will show as `name`. A NUL in it would end the name early on
    /// the wire, so none is kept.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.chars().filter(|c| *c != '\0').collect(),
            version: VERSION,
            enumerator: None,
            negotiated: false,
            refused: false,
            format: None,
            announced: false,
            devices: Vec::new(),
            withdrawn: Vec::new(),
            activations: 0,
            streaming: false,
            stream_channel: None,
            credits: 0,
            pending: VecDeque::new(),
            awaiting_keyframe: false,
            keyframe_asked: false,
        }
    }

    /// Whether a channel the host wants open by `name` is one of this end's: the
    /// enumeration channel always, and the device channel — as many times as the host
    /// likes — once the device has been announced.
    pub fn wants(&self, name: &str) -> bool {
        name == ENUMERATOR || (name == DEVICE_CHANNEL && self.announced)
    }

    /// Whether dynamic channel `channel` is one of this end's open channels.
    pub fn owns(&self, channel: u32) -> bool {
        self.enumerator == Some(channel) || self.instance(channel).is_some() || self.withdrawn.contains(&channel)
    }

    /// A channel [`Self::wants`] was accepted as `channel`.
    ///
    /// The enumeration channel opens the conversation with the version request (3.2.5.1),
    /// and a second one opened in its place starts it again. A device channel instance
    /// says nothing until asked.
    pub fn opened(&mut self, name: &str, channel: u32) -> Turn {
        let mut turn = Turn::default();
        if name == ENUMERATOR {
            debug!("rdp: the host opened camera enumeration on dynamic channel {channel}");
            self.enumerator = Some(channel);
            self.version = VERSION;
            self.negotiated = false;
            self.refused = false;
            self.announced = false;
            turn.reply(channel, vec![VERSION, SELECT_VERSION_REQUEST]);
        } else if name == DEVICE_CHANNEL && self.instance(channel).is_none() {
            debug!("rdp: the host opened the camera device on dynamic channel {channel}");
            self.withdrawn.retain(|withdrawn| *withdrawn != channel);
            if self.devices.is_empty() {
                turn.outputs.push(Output::Attached);
            }
            self.devices.push(Instance { channel, activations: 0 });
        }
        turn
    }

    /// The host closed `channel`. The enumeration channel takes the negotiation and the
    /// announcement with it; a device channel instance takes back the activations it made,
    /// and the stream if the stream was its or the device is left Deactivated.
    pub fn closed(&mut self, channel: u32) -> Turn {
        let mut turn = Turn::default();
        if self.enumerator == Some(channel) {
            debug!("rdp: the host closed camera enumeration");
            self.enumerator = None;
            self.negotiated = false;
            self.refused = false;
            self.announced = false;
        } else if let Some(at) = self.devices.iter().position(|instance| instance.channel == channel) {
            debug!("rdp: the host closed a camera device channel, {channel}");
            let instance = self.devices.remove(at);
            self.activations = self.activations.saturating_sub(instance.activations);
            if (self.stream_channel == Some(channel) || self.activations == 0) && self.stop_stream() {
                turn.outputs.push(Output::Stopped);
            }
        } else {
            self.withdrawn.retain(|withdrawn| *withdrawn != channel);
        }
        turn
    }

    /// One whole message on one of this end's channels.
    pub fn push(&mut self, channel: u32, message: &[u8]) -> Turn {
        let mut turn = Turn::default();
        if self.enumerator == Some(channel) {
            self.on_enumerator(message, &mut turn);
        } else if self.owns(channel) {
            self.on_device(channel, message, &mut turn);
        }
        turn
    }

    /// Plug the device in `format`, announcing it as soon as a version has been agreed.
    /// Plugging it again in the same format is nothing; in another, it is unplugged first,
    /// because the host holds the media type list against the device it was given for.
    pub fn plug(&mut self, format: Format) -> Turn {
        if !format.is_describable() {
            warn!("rdp: refusing to plug a camera in {format:?}, which is no media type");
            return Turn::default();
        }
        if self.format == Some(format) {
            return Turn::default();
        }
        let mut turn = self.unplug();
        self.format = Some(format);
        self.announce(&mut turn);
        turn
    }

    /// Withdraw the device: any Sample Request still owed is answered with an error, and
    /// the host is sent a Device Removed Notification (3.2.7). No [`Output::Stopped`],
    /// since the caller is the one ending it.
    pub fn unplug(&mut self) -> Turn {
        let mut turn = Turn::default();
        if self.format.take().is_none() {
            return turn;
        }
        if let (true, Some(channel)) = (self.streaming, self.stream_channel) {
            for _ in 0..self.credits {
                turn.reply(channel, self.sample_error(0, UNEXPECTED_ERROR));
            }
        }
        self.stop_stream();
        self.activations = 0;
        self.withdrawn.extend(self.devices.drain(..).map(|instance| instance.channel));
        if std::mem::take(&mut self.announced)
            && let Some(enumerator) = self.enumerator
        {
            turn.reply(enumerator, self.device_removed(DEVICE_CHANNEL));
        }
        turn
    }

    /// One encoded picture from the caller: sent if a Sample Request is owed, queued if
    /// not, and dropped — with everything after it until a keyframe — past [`PENDING`].
    /// Nothing is taken while the host is not streaming.
    pub fn sample(&mut self, sample: Vec<u8>, keyframe: bool) -> Turn {
        let mut turn = Turn::default();
        let (true, Some(channel)) = (self.streaming, self.stream_channel) else {
            return turn;
        };
        if self.awaiting_keyframe {
            if !keyframe {
                self.ask_for_keyframe(&mut turn);
                return turn;
            }
            self.awaiting_keyframe = false;
            self.keyframe_asked = false;
        }
        if self.credits > 0 && self.pending.is_empty() {
            self.credits -= 1;
            turn.reply(channel, self.sample_response(&sample));
        } else if self.pending.len() < PENDING {
            self.pending.push_back(sample);
        } else {
            debug!("rdp: {PENDING} camera samples waited for the host; dropping to a keyframe");
            self.pending.clear();
            self.awaiting_keyframe = true;
            self.ask_for_keyframe(&mut turn);
        }
        turn
    }

    fn instance(&self, channel: u32) -> Option<&Instance> {
        self.devices.iter().find(|instance| instance.channel == channel)
    }

    fn ask_for_keyframe(&mut self, turn: &mut Turn) {
        if !std::mem::replace(&mut self.keyframe_asked, true) {
            turn.outputs.push(Output::KeyframeNeeded);
        }
    }

    /// End a stream, forgetting everything metered. Whether one was running.
    fn stop_stream(&mut self) -> bool {
        self.stream_channel = None;
        self.credits = 0;
        self.pending.clear();
        self.awaiting_keyframe = false;
        self.keyframe_asked = false;
        std::mem::take(&mut self.streaming)
    }

    /// Send the Device Added Notification once everything it waits for holds.
    fn announce(&mut self, turn: &mut Turn) {
        if let (Some(enumerator), true, false, true) =
            (self.enumerator, self.negotiated, self.announced, self.format.is_some())
        {
            turn.reply(enumerator, self.device_added(&self.name, DEVICE_CHANNEL));
            self.announced = true;
        }
    }

    /// The enumeration channel carries one message for this end: the version response.
    fn on_enumerator(&mut self, message: &[u8], turn: &mut Turn) {
        match message {
            &[version, SELECT_VERSION_RESPONSE] if !self.negotiated && !self.refused => {
                if (1..=VERSION).contains(&version) {
                    self.version = version;
                    self.negotiated = true;
                    turn.outputs.push(Output::Negotiated { version });
                    self.announce(turn);
                } else {
                    self.refused = true;
                    turn.outputs.push(Output::VersionRefused { version });
                }
            }
            _ => debug!(
                "rdp: discarding a {}-byte camera enumeration message the conversation has no \
                 place for",
                message.len()
            ),
        }
    }

    /// A device channel instance: one request, one response, on that instance.
    fn on_device(&mut self, channel: u32, message: &[u8], turn: &mut Turn) {
        let (version, id, body) = match message {
            [version, id, body @ ..] => (*version, *id, body),
            _ => {
                debug!("rdp: a {}-byte camera message has no header", message.len());
                turn.reply(channel, self.error(INVALID_MESSAGE));
                return;
            }
        };
        if matches!(
            id,
            SUCCESS_RESPONSE
                | ERROR_RESPONSE
                | SELECT_VERSION_REQUEST
                | SELECT_VERSION_RESPONSE
                | DEVICE_ADDED_NOTIFICATION
                | DEVICE_REMOVED_NOTIFICATION
                | STREAM_LIST_RESPONSE
                | MEDIA_TYPE_LIST_RESPONSE
                | CURRENT_MEDIA_TYPE_RESPONSE
                | SAMPLE_RESPONSE
                | SAMPLE_ERROR_RESPONSE
                | PROPERTY_LIST_RESPONSE
                | PROPERTY_VALUE_RESPONSE
        ) {
            // Not a request, so nothing is owed for it, and answering one is how two
            // ends volley errors at each other with no last word.
            debug!("rdp: discarding camera message {id:#04x}, which is not a request");
            return;
        }
        debug!("rdp: the host asks the camera {id:#04x} on channel {channel} ({} body bytes)", body.len());
        let reply = self.request(channel, version, id, body, turn);
        // Empty for a Sample Request owed until there is a sample: it is answered when
        // one arrives.
        if !reply.is_empty() {
            turn.reply(channel, reply);
        }
    }

    /// The response a request earns, after the checks 3.1.1 and 3.2.5 put in front of
    /// acting on it: that it is well formed for the negotiated version, that it is for the
    /// device as it is plugged now, and that the device's state takes it.
    fn request(&mut self, channel: u32, version: u8, id: u8, body: &[u8], turn: &mut Turn) -> Vec<u8> {
        // A Sample Request's every failure is a Sample Error Response, naming the stream
        // it asked about (3.2.5.23, 3.2.5.24).
        let fail = |this: &Self, code: u32| match id {
            SAMPLE_REQUEST => this.sample_error(body.first().copied().unwrap_or(0), code),
            _ => this.error(code),
        };
        let properties = self.version >= 2;
        let well_formed = version == self.version
            && match id {
                ACTIVATE_DEVICE_REQUEST
                | DEACTIVATE_DEVICE_REQUEST
                | STREAM_LIST_REQUEST
                | STOP_STREAMS_REQUEST => body.is_empty(),
                MEDIA_TYPE_LIST_REQUEST | CURRENT_MEDIA_TYPE_REQUEST | SAMPLE_REQUEST => body.len() == 1,
                START_STREAMS_REQUEST => {
                    !body.is_empty()
                        && body.len().is_multiple_of(START_STREAM_INFO_BYTES)
                        && body.len() / START_STREAM_INFO_BYTES <= usize::from(u8::MAX)
                }
                PROPERTY_LIST_REQUEST => properties && body.is_empty(),
                PROPERTY_VALUE_REQUEST => properties && body.len() == 2,
                SET_PROPERTY_VALUE_REQUEST => {
                    properties
                        && body.len() == 7
                        && matches!(body[2], PROPERTY_MODE_MANUAL | PROPERTY_MODE_AUTO)
                }
                _ => false,
            };
        if !well_formed {
            debug!("rdp: refusing camera request {id:#04x} ({} body bytes) as malformed", body.len());
            return fail(self, INVALID_MESSAGE);
        }
        let (Some(format), Some(at)) =
            (self.format, self.devices.iter().position(|instance| instance.channel == channel))
        else {
            debug!("rdp: refusing camera request {id:#04x} for a device that has been withdrawn");
            return fail(self, INVALID_REQUEST);
        };
        if self.activations == 0 && id != ACTIVATE_DEVICE_REQUEST {
            return fail(self, NOT_INITIALIZED);
        }
        match id {
            ACTIVATE_DEVICE_REQUEST => match self.activations.checked_add(1) {
                Some(activations) => {
                    self.activations = activations;
                    let instance = &mut self.devices[at];
                    instance.activations = instance.activations.saturating_add(1);
                    self.success()
                }
                None => self.error(INVALID_REQUEST),
            },
            DEACTIVATE_DEVICE_REQUEST => {
                if self.stop_stream() {
                    turn.outputs.push(Output::Stopped);
                }
                self.activations -= 1;
                let instance = &mut self.devices[at];
                instance.activations = instance.activations.saturating_sub(1);
                self.success()
            }
            STREAM_LIST_REQUEST => self.stream_list(),
            MEDIA_TYPE_LIST_REQUEST | CURRENT_MEDIA_TYPE_REQUEST if body[0] != 0 => {
                self.error(INVALID_STREAM_NUMBER)
            }
            MEDIA_TYPE_LIST_REQUEST => self.media_type_response(MEDIA_TYPE_LIST_RESPONSE, format),
            CURRENT_MEDIA_TYPE_REQUEST => self.media_type_response(CURRENT_MEDIA_TYPE_RESPONSE, format),
            START_STREAMS_REQUEST => self.start_streams(channel, body, format, turn),
            STOP_STREAMS_REQUEST => {
                if self.stop_stream() {
                    turn.outputs.push(Output::Stopped);
                }
                self.success()
            }
            SAMPLE_REQUEST if body[0] != 0 => self.sample_error(body[0], INVALID_STREAM_NUMBER),
            SAMPLE_REQUEST if !self.streaming => self.sample_error(0, INVALID_REQUEST),
            SAMPLE_REQUEST => {
                self.stream_channel = Some(channel);
                self.owe_sample()
            }
            PROPERTY_LIST_REQUEST => vec![self.version, PROPERTY_LIST_RESPONSE],
            PROPERTY_VALUE_REQUEST | SET_PROPERTY_VALUE_REQUEST => match body[0] {
                PROPERTY_SET_CAMERA_CONTROL | PROPERTY_SET_VIDEO_PROC_AMP => self.error(ITEM_NOT_FOUND),
                _ => self.error(SET_NOT_FOUND),
            },
            _ => unreachable!("every other id was refused as malformed"),
        }
    }

    /// A Start Streams Request: every START_STREAM_INFO must name stream 0, once, in the
    /// one media type the list offered.
    fn start_streams(&mut self, channel: u32, body: &[u8], format: Format, turn: &mut Turn) -> Vec<u8> {
        let offered = format.media_type();
        for (index, info) in body.as_chunks::<START_STREAM_INFO_BYTES>().0.iter().enumerate() {
            if info[0] != 0 {
                return self.error(INVALID_STREAM_NUMBER);
            }
            if index > 0 {
                return self.error(INVALID_REQUEST); // stream 0, named twice
            }
            if info[1..] != offered[..] {
                return self.error(INVALID_MEDIA_TYPE);
            }
        }
        // A stream started over a running one replaces it, and a new stream opens on a
        // keyframe: the host's decoder starts from parameter sets.
        self.stop_stream();
        self.streaming = true;
        self.stream_channel = Some(channel);
        self.awaiting_keyframe = true;
        turn.outputs.push(Output::Started(format));
        self.success()
    }

    /// A Sample Request while streaming: answered with the oldest waiting sample, or owed
    /// until the caller has one.
    fn owe_sample(&mut self) -> Vec<u8> {
        match self.pending.pop_front() {
            Some(sample) => self.sample_response(&sample),
            None => {
                self.credits = self.credits.saturating_add(1);
                Vec::new()
            }
        }
    }

    fn success(&self) -> Vec<u8> {
        vec![self.version, SUCCESS_RESPONSE]
    }

    fn error(&self, code: u32) -> Vec<u8> {
        let mut w = Writer::with_capacity(6);
        w.u8(self.version);
        w.u8(ERROR_RESPONSE);
        w.u32_le(code);
        w.finish()
    }

    fn sample_error(&self, stream: u8, code: u32) -> Vec<u8> {
        let mut w = Writer::with_capacity(7);
        w.u8(self.version);
        w.u8(SAMPLE_ERROR_RESPONSE);
        w.u8(stream);
        w.u32_le(code);
        w.finish()
    }

    fn sample_response(&self, sample: &[u8]) -> Vec<u8> {
        let mut w = Writer::with_capacity(3 + sample.len());
        w.u8(self.version);
        w.u8(SAMPLE_RESPONSE);
        w.u8(0); // StreamIndex
        w.bytes(sample);
        w.finish()
    }

    /// One stream: color, capture, selected, and not shared between the host's
    /// applications — the one encoder behind it is configured for one consumer, and the
    /// shape FreeRDP's client sends a Windows host is the same.
    fn stream_list(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(7);
        w.u8(self.version);
        w.u8(STREAM_LIST_RESPONSE);
        w.u16_le(FRAME_SOURCE_COLOR);
        w.u8(STREAM_CATEGORY_CAPTURE);
        w.u8(1); // Selected
        w.u8(0); // CanBeShared
        w.finish()
    }

    /// A Media Type List Response of one, or a Current Media Type Response: the same
    /// description behind a different id.
    fn media_type_response(&self, id: u8, format: Format) -> Vec<u8> {
        let mut w = Writer::with_capacity(2 + MEDIA_TYPE_BYTES);
        w.u8(self.version);
        w.u8(id);
        w.bytes(&format.media_type());
        w.finish()
    }

    /// DeviceName as null-terminated UTF-16, then the channel's name as null-terminated
    /// ANSI (2.2.2.3).
    fn device_added(&self, name: &str, channel: &str) -> Vec<u8> {
        debug_assert!(channel.is_ascii() && channel.len() <= MAX_CHANNEL_NAME);
        let mut w = Writer::with_capacity(2 + 2 * (name.len() + 1) + channel.len() + 1);
        w.u8(self.version);
        w.u8(DEVICE_ADDED_NOTIFICATION);
        for unit in name.encode_utf16() {
            w.u16_le(unit);
        }
        w.u16_le(0);
        w.bytes(channel.as_bytes());
        w.u8(0);
        w.finish()
    }

    fn device_removed(&self, channel: &str) -> Vec<u8> {
        let mut w = Writer::with_capacity(2 + channel.len() + 1);
        w.u8(self.version);
        w.u8(DEVICE_REMOVED_NOTIFICATION);
        w.bytes(channel.as_bytes());
        w.u8(0);
        w.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENUMERATION: u32 = 5;
    const DEVICE: u32 = 6;

    const VGA: Format = Format { width: 640, height: 480, fps_numerator: 30, fps_denominator: 1 };
    const FULL_HD: Format = Format { width: 1920, height: 1080, fps_numerator: 30, fps_denominator: 1 };

    /// A camera the host has negotiated with, been told about, and opened the channel of.
    fn attached(format: Format) -> Rdpecam {
        let mut cam = Rdpecam::new("Remotex Camera");
        cam.opened(ENUMERATOR, ENUMERATION);
        cam.push(ENUMERATION, &[2, SELECT_VERSION_RESPONSE]);
        cam.plug(format);
        assert!(cam.wants(DEVICE_CHANNEL));
        assert_eq!(cam.opened(DEVICE_CHANNEL, DEVICE).outputs, vec![Output::Attached]);
        cam
    }

    /// The replies one request on a device channel instance earned, which all go back on it.
    fn ask_on(cam: &mut Rdpecam, channel: u32, request: &[u8]) -> Vec<Vec<u8>> {
        let turn = cam.push(channel, request);
        turn.replies
            .into_iter()
            .map(|(to, reply)| {
                assert_eq!(to, channel, "a device reply went to another channel");
                reply
            })
            .collect()
    }

    fn ask(cam: &mut Rdpecam, request: &[u8]) -> Vec<Vec<u8>> {
        ask_on(cam, DEVICE, request)
    }

    fn start(format: Format) -> Vec<u8> {
        let mut start = vec![2, START_STREAMS_REQUEST, 0];
        start.extend_from_slice(&format.media_type());
        start
    }

    fn streaming(format: Format) -> Rdpecam {
        let mut cam = attached(format);
        ask(&mut cam, &[2, ACTIVATE_DEVICE_REQUEST]);
        assert_eq!(ask(&mut cam, &start(format)), vec![vec![2, SUCCESS_RESPONSE]]);
        cam
    }

    /// 4.1: the version request is the first message, and the response settles it.
    #[test]
    fn the_enumeration_channel_opens_with_the_version_request() {
        let mut cam = Rdpecam::new("Remotex Camera");
        assert!(cam.wants(ENUMERATOR));
        assert!(!cam.wants(DEVICE_CHANNEL), "no device channel before one is announced");
        let turn = cam.opened(ENUMERATOR, ENUMERATION);
        assert_eq!(turn.replies, vec![(ENUMERATION, vec![0x02, 0x03])]);
        let turn = cam.push(ENUMERATION, &[0x02, 0x04]);
        assert_eq!(turn.outputs, vec![Output::Negotiated { version: 2 }]);
        assert!(turn.replies.is_empty(), "nothing is plugged, so nothing is announced");
    }

    /// 4.2.1, byte for byte.
    #[test]
    fn a_device_added_notification_is_the_specifications_example() {
        let cam = Rdpecam::new("Mock Camera 1");
        let expected = [
            0x02, 0x05, 0x4d, 0x00, 0x6f, 0x00, 0x63, 0x00, 0x6b, 0x00, 0x20, 0x00, 0x43, 0x00, 0x61, 0x00,
            0x6d, 0x00, 0x65, 0x00, 0x72, 0x00, 0x61, 0x00, 0x20, 0x00, 0x31, 0x00, 0x00, 0x00, 0x52, 0x44,
            0x43, 0x61, 0x6d, 0x65, 0x72, 0x61, 0x5f, 0x44, 0x65, 0x76, 0x69, 0x63, 0x65, 0x5f, 0x30, 0x00,
        ];
        assert_eq!(cam.device_added("Mock Camera 1", "RDCamera_Device_0"), expected);
    }

    /// 4.3.1, byte for byte.
    #[test]
    fn a_device_removed_notification_is_the_specifications_example() {
        let cam = Rdpecam::new("Mock Camera 1");
        let expected = [
            0x02, 0x06, 0x52, 0x44, 0x43, 0x61, 0x6d, 0x65, 0x72, 0x61, 0x5f, 0x44, 0x65, 0x76, 0x69, 0x63,
            0x65, 0x5f, 0x31, 0x00,
        ];
        assert_eq!(cam.device_removed("RDCamera_Device_1"), expected);
    }

    /// A device plugged before the version is agreed is announced the moment it is, and
    /// one plugged after is announced at once.
    #[test]
    fn the_device_is_announced_once_a_version_is_agreed() {
        let mut cam = Rdpecam::new("Cam");
        cam.opened(ENUMERATOR, ENUMERATION);
        assert!(cam.plug(VGA).replies.is_empty());
        let turn = cam.push(ENUMERATION, &[2, SELECT_VERSION_RESPONSE]);
        let added = cam.device_added("Cam", DEVICE_CHANNEL);
        assert_eq!(turn.replies, vec![(ENUMERATION, added.clone())]);
        assert_eq!(&added[..10], &[2, 0x05, b'C', 0, b'a', 0, b'm', 0, 0, 0]);
        assert_eq!(&added[10..], b"Remotex_Camera_0\0");
        assert!(cam.plug(VGA).replies.is_empty(), "plugging again in the same format is nothing");
    }

    /// 3.2.5.2: a version this end does not speak ends the protocol there, and a second
    /// response is out of sequence and discarded.
    #[test]
    fn a_version_this_end_does_not_speak_stops_the_protocol() {
        for version in [0, 3] {
            let mut cam = Rdpecam::new("Cam");
            cam.opened(ENUMERATOR, ENUMERATION);
            cam.plug(VGA);
            let turn = cam.push(ENUMERATION, &[version, SELECT_VERSION_RESPONSE]);
            assert_eq!(turn.outputs, vec![Output::VersionRefused { version }]);
            assert!(turn.replies.is_empty());
            let turn = cam.push(ENUMERATION, &[2, SELECT_VERSION_RESPONSE]);
            assert_eq!(turn, Turn::default(), "the conversation on this channel is over");
            assert!(!cam.wants(DEVICE_CHANNEL));
        }
    }

    /// 4.4: the Device Initialization sequence, answered with the specification's own bytes
    /// where it shows them.
    #[test]
    fn the_device_initialization_sequence_is_answered() {
        let mut cam = attached(VGA);
        assert_eq!(ask(&mut cam, &[0x02, 0x07]), vec![vec![0x02, 0x01]]);
        // 4.4.4's first description, except that this stream is not shareable.
        assert_eq!(ask(&mut cam, &[0x02, 0x09]), vec![vec![0x02, 0x0a, 0x01, 0x00, 0x01, 0x01, 0x00]]);
        // The first entry of 4.4.6's list, which is this device's whole list.
        let list = [
            0x02, 0x0c, 0x01, 0x80, 0x02, 0x00, 0x00, 0xe0, 0x01, 0x00, 0x00, 0x1e, 0x00, 0x00, 0x00, 0x01,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        ];
        assert_eq!(ask(&mut cam, &[0x02, 0x0b, 0x00]), vec![list.to_vec()]);
        assert_eq!(ask(&mut cam, &[0x02, 0x08]), vec![vec![0x02, 0x01]]);

        // 4.4.8 is a 1080p device's current media type.
        let mut cam = attached(FULL_HD);
        ask(&mut cam, &[0x02, 0x07]);
        let current = [
            0x02, 0x0e, 0x01, 0x80, 0x07, 0x00, 0x00, 0x38, 0x04, 0x00, 0x00, 0x1e, 0x00, 0x00, 0x00, 0x01,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        ];
        assert_eq!(ask(&mut cam, &[0x02, 0x0d, 0x00]), vec![current.to_vec()]);
        assert_eq!(ask(&mut cam, &[0x02, 0x0d, 0x01]), vec![cam.error(INVALID_STREAM_NUMBER)]);
    }

    /// 4.5: the Video Capture sequence with 4.5.1's request, and a sample going out for the
    /// request that was owed.
    #[test]
    fn the_video_capture_sequence_streams_a_sample_per_request() {
        let mut cam = attached(FULL_HD);
        ask(&mut cam, &[0x02, 0x07]);
        let start = [
            0x02, 0x0f, 0x00, 0x01, 0x80, 0x07, 0x00, 0x00, 0x38, 0x04, 0x00, 0x00, 0x1e, 0x00, 0x00, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        ];
        let turn = cam.push(DEVICE, &start);
        assert_eq!(turn.replies, vec![(DEVICE, vec![0x02, 0x01])]);
        assert_eq!(turn.outputs, vec![Output::Started(FULL_HD)]);

        assert!(ask(&mut cam, &[0x02, 0x11, 0x00]).is_empty(), "owed until there is a sample");
        let turn = cam.sample(vec![0, 0, 0, 1, 0x09, 0x30], true);
        assert_eq!(turn.replies, vec![(DEVICE, vec![0x02, 0x12, 0x00, 0, 0, 0, 1, 0x09, 0x30])]);
        assert!(cam.sample(vec![7], false).replies.is_empty(), "nothing is owed, so it waits");
        assert_eq!(ask(&mut cam, &[0x02, 0x11, 0x00]), vec![vec![0x02, 0x12, 0x00, 7]]);

        let turn = cam.push(DEVICE, &[0x02, 0x10]);
        assert_eq!(turn.replies, vec![(DEVICE, vec![0x02, 0x01])]);
        assert_eq!(turn.outputs, vec![Output::Stopped]);
        assert!(cam.sample(vec![1], true).replies.is_empty(), "nothing is taken once stopped");
    }

    /// 3.1.1 and 4.8: a Deactivated device takes nothing but activation, and says so with
    /// NotInitialized — a Sample Request in the Sample Error Response it is always owed.
    #[test]
    fn a_deactivated_device_answers_not_initialized() {
        let mut cam = attached(VGA);
        assert_eq!(ask(&mut cam, &[0x02, 0x09]), vec![vec![0x02, 0x02, 0x03, 0x00, 0x00, 0x00]]);
        assert_eq!(ask(&mut cam, &[0x02, 0x11, 0x00]), vec![vec![0x02, 0x13, 0x00, 0x03, 0x00, 0x00, 0x00]]);
        let turn = cam.push(DEVICE, &start(VGA));
        assert_eq!(turn.replies, vec![(DEVICE, cam.error(NOT_INITIALIZED))]);
        assert!(turn.outputs.is_empty());
    }

    /// An Activated device that is not streaming owes no samples: a Sample Request there is
    /// invalid in the current state.
    #[test]
    fn a_sample_request_before_the_stream_starts_is_an_invalid_request() {
        let mut cam = attached(VGA);
        ask(&mut cam, &[2, ACTIVATE_DEVICE_REQUEST]);
        assert_eq!(ask(&mut cam, &[2, SAMPLE_REQUEST, 0]), vec![vec![2, 0x13, 0, 0x04, 0, 0, 0]]);
        let mut cam = streaming(VGA);
        assert_eq!(ask(&mut cam, &[2, SAMPLE_REQUEST, 1]), vec![vec![2, 0x13, 1, 0x05, 0, 0, 0]]);
    }

    /// Activations nest (3.1.1): the device is not Deactivated until every Activate has its
    /// Deactivate, and a Deactivate ends a stream either way.
    #[test]
    fn activations_nest_and_a_deactivate_ends_the_stream() {
        let mut cam = streaming(VGA);
        ask(&mut cam, &[2, ACTIVATE_DEVICE_REQUEST]);
        let turn = cam.push(DEVICE, &[2, DEACTIVATE_DEVICE_REQUEST]);
        assert_eq!(turn.replies, vec![(DEVICE, vec![2, SUCCESS_RESPONSE])]);
        assert_eq!(turn.outputs, vec![Output::Stopped]);
        assert_ne!(ask(&mut cam, &[2, STREAM_LIST_REQUEST]), vec![cam.error(NOT_INITIALIZED)], "still activated once");
        ask(&mut cam, &[2, DEACTIVATE_DEVICE_REQUEST]);
        assert_eq!(ask(&mut cam, &[2, STREAM_LIST_REQUEST]), vec![cam.error(NOT_INITIALIZED)]);
    }

    /// What a Windows host was measured doing: the Device Initialization sequence on one
    /// instance of the device channel, then — with that instance still open and activated —
    /// a second instance for Device Control Initialization. Each is answered on its own
    /// channel, the activations nest across both, and the stream belongs to the instance
    /// that started it.
    #[test]
    fn two_instances_of_the_device_channel_share_the_device() {
        const CONTROL: u32 = 19;
        let mut cam = attached(VGA);
        for request in [&[2, ACTIVATE_DEVICE_REQUEST][..], &[2, STREAM_LIST_REQUEST], &[2, MEDIA_TYPE_LIST_REQUEST, 0], &[2, CURRENT_MEDIA_TYPE_REQUEST, 0]] {
            assert_eq!(ask(&mut cam, request).len(), 1);
        }
        assert!(cam.wants(DEVICE_CHANNEL), "a second instance is as welcome as the first");
        assert!(cam.opened(DEVICE_CHANNEL, CONTROL).outputs.is_empty(), "the device was already attached");
        assert_eq!(ask_on(&mut cam, CONTROL, &[2, ACTIVATE_DEVICE_REQUEST]), vec![vec![2, SUCCESS_RESPONSE]]);
        assert_eq!(ask_on(&mut cam, CONTROL, &[2, PROPERTY_LIST_REQUEST]), vec![vec![2, PROPERTY_LIST_RESPONSE]]);

        let turn = cam.push(DEVICE, &start(VGA));
        assert_eq!(turn.outputs, vec![Output::Started(VGA)]);
        assert!(ask(&mut cam, &[2, SAMPLE_REQUEST, 0]).is_empty());
        assert_eq!(cam.sample(vec![9], true).replies, vec![(DEVICE, vec![2, SAMPLE_RESPONSE, 0, 9])]);

        // The control instance deactivating ends the stream, as a Deactivate does, but
        // leaves the device activated by the other.
        let turn = cam.push(CONTROL, &[2, DEACTIVATE_DEVICE_REQUEST]);
        assert_eq!(turn.outputs, vec![Output::Stopped]);
        let turn = cam.push(DEVICE, &start(VGA));
        assert_eq!(turn.replies, vec![(DEVICE, vec![2, SUCCESS_RESPONSE])]);

        // An instance that is not the stream's closing leaves the stream alone; the
        // stream's own closing ends it, and takes its activation with it.
        ask_on(&mut cam, CONTROL, &[2, ACTIVATE_DEVICE_REQUEST]);
        assert!(cam.closed(CONTROL).outputs.is_empty());
        assert_eq!(cam.closed(DEVICE).outputs, vec![Output::Stopped]);
        assert_eq!(cam.opened(DEVICE_CHANNEL, 20).outputs, vec![Output::Attached]);
        assert_eq!(ask_on(&mut cam, 20, &[2, STREAM_LIST_REQUEST]), vec![cam.error(NOT_INITIALIZED)]);
    }

    /// 3.2.5: a malformed request — the wrong length, the wrong version, an id that is no
    /// request — is answered InvalidMessage; a response from the host is not answered.
    #[test]
    fn malformed_requests_are_answered_invalid_message_and_responses_not_at_all() {
        let mut cam = attached(VGA);
        let invalid = cam.error(INVALID_MESSAGE);
        for request in [
            &[2, ACTIVATE_DEVICE_REQUEST, 0][..],
            &[1, ACTIVATE_DEVICE_REQUEST],
            &[2, MEDIA_TYPE_LIST_REQUEST],
            &[2, 0x19],
            &[2],
            &[],
        ] {
            assert_eq!(ask(&mut cam, request), vec![invalid.clone()], "{request:?}");
        }
        assert_eq!(ask(&mut cam, &[2, SAMPLE_REQUEST]), vec![vec![2, 0x13, 0, 0x02, 0, 0, 0]]);
        for response in [&[2, SUCCESS_RESPONSE][..], &[2, ERROR_RESPONSE, 1, 0, 0, 0], &[2, SAMPLE_RESPONSE, 0, 1]] {
            assert!(ask(&mut cam, response).is_empty(), "{response:?}");
        }
    }

    /// The host picks from the list it was given: one stream, once, in the one format.
    #[test]
    fn a_start_streams_outside_the_list_is_refused() {
        let mut cam = attached(VGA);
        ask(&mut cam, &[2, ACTIVATE_DEVICE_REQUEST]);
        let info = |index: u8, format: Format| {
            let mut info = vec![index];
            info.extend_from_slice(&format.media_type());
            info
        };
        let starting = |infos: &[Vec<u8>]| {
            let mut request = vec![2, START_STREAMS_REQUEST];
            for info in infos {
                request.extend_from_slice(info);
            }
            request
        };
        assert_eq!(ask(&mut cam, &starting(&[info(0, FULL_HD)])), vec![cam.error(INVALID_MEDIA_TYPE)]);
        assert_eq!(ask(&mut cam, &starting(&[info(1, VGA)])), vec![cam.error(INVALID_STREAM_NUMBER)]);
        assert_eq!(ask(&mut cam, &starting(&[info(0, VGA), info(0, VGA)])), vec![cam.error(INVALID_REQUEST)]);
        let mut truncated = starting(&[info(0, VGA)]);
        truncated.pop();
        assert_eq!(ask(&mut cam, &truncated), vec![cam.error(INVALID_MESSAGE)]);
        let mut flagged = info(0, VGA);
        flagged[26] |= 0x02; // BottomUpImage: not the media type that was offered
        let turn = cam.push(DEVICE, &starting(&[flagged]));
        assert_eq!(turn.replies, vec![(DEVICE, cam.error(INVALID_MEDIA_TYPE))]);
        assert!(turn.outputs.is_empty());
    }

    /// A stream starts at a keyframe, a queue that outgrows [`PENDING`] is dropped whole,
    /// and the caller is asked for a keyframe once per gap.
    #[test]
    fn samples_wait_for_requests_and_a_gap_waits_for_a_keyframe() {
        let mut cam = streaming(VGA);
        let turn = cam.sample(vec![1], false);
        assert_eq!(turn.outputs, vec![Output::KeyframeNeeded], "a stream opens on a keyframe");
        assert_eq!(cam.sample(vec![2], false), Turn::default(), "asked once per gap");

        for n in 0..PENDING {
            let turn = cam.sample(vec![10 + n as u8], n == 0);
            assert_eq!(turn, Turn::default(), "sample {n} waits for a request");
        }
        let turn = cam.sample(vec![99], false);
        assert_eq!(turn.outputs, vec![Output::KeyframeNeeded]);
        assert!(ask(&mut cam, &[2, SAMPLE_REQUEST, 0]).is_empty(), "the queue went with the gap");
        assert_eq!(cam.sample(vec![3], false), Turn::default(), "still waiting for a keyframe");
        let turn = cam.sample(vec![4], true);
        assert_eq!(turn.replies, vec![(DEVICE, vec![2, SAMPLE_RESPONSE, 0, 4])], "the owed request takes it");
        cam.sample(vec![5], false);
        cam.sample(vec![6], false);
        assert_eq!(ask(&mut cam, &[2, SAMPLE_REQUEST, 0]), vec![vec![2, SAMPLE_RESPONSE, 0, 5]], "oldest first");
    }

    /// Version 2's properties: an empty list and no property found; under version 1 none
    /// of it exists.
    #[test]
    fn this_device_has_no_properties_and_version_1_has_no_property_requests() {
        let mut cam = attached(VGA);
        ask(&mut cam, &[2, ACTIVATE_DEVICE_REQUEST]);
        assert_eq!(ask(&mut cam, &[0x02, 0x14]), vec![vec![0x02, 0x15]]);
        assert_eq!(ask(&mut cam, &[0x02, 0x16, 0x02, 0x02]), vec![cam.error(ITEM_NOT_FOUND)]);
        assert_eq!(ask(&mut cam, &[0x02, 0x18, 0x02, 0x02, 0x01, 0x64, 0, 0, 0]), vec![cam.error(ITEM_NOT_FOUND)]);
        assert_eq!(ask(&mut cam, &[0x02, 0x16, 0x07, 0x02]), vec![cam.error(SET_NOT_FOUND)]);
        assert_eq!(ask(&mut cam, &[0x02, 0x18, 0x02, 0x02, 0x03, 0x64, 0, 0, 0]), vec![cam.error(INVALID_MESSAGE)]);

        let mut old = Rdpecam::new("Cam");
        old.opened(ENUMERATOR, ENUMERATION);
        old.push(ENUMERATION, &[1, SELECT_VERSION_RESPONSE]);
        let turn = old.plug(VGA);
        assert_eq!(turn.replies[0].1[0], 1, "every message after the negotiation carries its version");
        old.opened(DEVICE_CHANNEL, DEVICE);
        assert_eq!(ask(&mut old, &[1, ACTIVATE_DEVICE_REQUEST]), vec![vec![1, SUCCESS_RESPONSE]]);
        assert_eq!(ask(&mut old, &[1, PROPERTY_LIST_REQUEST]), vec![vec![1, ERROR_RESPONSE, 2, 0, 0, 0]]);
    }

    /// Unplugging answers every Sample Request still owed, then tells the host the device is
    /// gone; the host cannot open it again until it is plugged again, and what is still
    /// open for it answers InvalidRequest until the host closes it.
    #[test]
    fn unplugging_settles_what_is_owed_and_removes_the_device() {
        let mut cam = streaming(VGA);
        ask(&mut cam, &[2, SAMPLE_REQUEST, 0]);
        ask(&mut cam, &[2, SAMPLE_REQUEST, 0]);
        let turn = cam.unplug();
        let owed = vec![2, SAMPLE_ERROR_RESPONSE, 0, 0x01, 0, 0, 0];
        assert_eq!(
            turn.replies,
            vec![(DEVICE, owed.clone()), (DEVICE, owed), (ENUMERATION, cam.device_removed(DEVICE_CHANNEL))]
        );
        assert!(turn.outputs.is_empty(), "the caller is the one ending it");
        assert!(!cam.wants(DEVICE_CHANNEL));
        assert_eq!(ask(&mut cam, &[2, STREAM_LIST_REQUEST]), vec![cam.error(INVALID_REQUEST)]);
        assert_eq!(ask(&mut cam, &[2, SAMPLE_REQUEST, 0]), vec![cam.sample_error(0, INVALID_REQUEST)]);
        assert_eq!(cam.closed(DEVICE), Turn::default());
        assert!(!cam.owns(DEVICE));
        assert_eq!(cam.unplug(), Turn::default());
    }

    /// Plugging in a new format withdraws the old device before announcing the new one, and
    /// the new device starts Deactivated on whatever channel the host opens for it.
    #[test]
    fn a_new_format_is_a_new_device() {
        let mut cam = attached(VGA);
        ask(&mut cam, &[2, ACTIVATE_DEVICE_REQUEST]);
        let turn = cam.plug(FULL_HD);
        assert_eq!(
            turn.replies,
            vec![
                (ENUMERATION, cam.device_removed(DEVICE_CHANNEL)),
                (ENUMERATION, cam.device_added("Remotex Camera", DEVICE_CHANNEL)),
            ]
        );
        assert_eq!(cam.opened(DEVICE_CHANNEL, 9).outputs, vec![Output::Attached]);
        assert!(cam.owns(9) && cam.owns(DEVICE), "the old instance is open until the host closes it");
        assert_eq!(ask(&mut cam, &[2, STREAM_LIST_REQUEST]), vec![cam.error(INVALID_REQUEST)]);
        assert_eq!(ask_on(&mut cam, 9, &[2, STREAM_LIST_REQUEST]), vec![cam.error(NOT_INITIALIZED)]);
    }

    /// The channels closing: the device's takes the stream, the enumerator's the
    /// negotiation, and a new enumerator starts the conversation again.
    #[test]
    fn closed_channels_take_their_state_with_them() {
        let mut cam = streaming(VGA);
        assert_eq!(cam.closed(DEVICE).outputs, vec![Output::Stopped]);
        assert!(!cam.owns(DEVICE));
        assert_eq!(cam.push(DEVICE, &[2, ACTIVATE_DEVICE_REQUEST]), Turn::default(), "no longer ours");

        assert_eq!(cam.closed(ENUMERATION), Turn::default());
        assert!(!cam.wants(DEVICE_CHANNEL), "the announcement went with the channel");
        let turn = cam.opened(ENUMERATOR, 11);
        assert_eq!(turn.replies, vec![(11, vec![2, SELECT_VERSION_REQUEST])]);
        let turn = cam.push(11, &[2, SELECT_VERSION_RESPONSE]);
        assert_eq!(turn.replies, vec![(11, cam.device_added("Remotex Camera", DEVICE_CHANNEL))]);
    }

    #[test]
    fn a_format_that_is_no_media_type_is_not_plugged() {
        let mut cam = Rdpecam::new("Cam");
        cam.opened(ENUMERATOR, ENUMERATION);
        cam.push(ENUMERATION, &[2, SELECT_VERSION_RESPONSE]);
        let broken = Format { fps_denominator: 0, ..VGA };
        assert_eq!(cam.plug(broken), Turn::default());
        assert!(!cam.wants(DEVICE_CHANNEL));
    }
}
