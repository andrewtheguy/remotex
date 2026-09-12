//! The conference the connection is joining, and what the two sides tell each other.
//!
//! T.124 calls it a conference: the client asks to create one, the server answers, and
//! inside both messages is an opaque block of "user data" that the conference protocol
//! never looks at. RDP puts everything it needs to agree before a session exists into
//! that block — the desktop size, the colour depth, which security the negotiation
//! settled on, and the static virtual channels the client wants.
//!
//! So this module is two things at once: a thin T.124 envelope, PER-encoded (see
//! [`super::per`]), and the RDP blocks inside it, which are ordinary little-endian RDP
//! structures. The envelope is nearly constant — the same bytes on every connection
//! ever made, bar two lengths — and the blocks are where the connection is actually
//! described.
//!
//! # What is asked for
//!
//! Three client blocks, which is the fewest a Windows host will accept: `CS_CORE`
//! describes the client, `CS_SECURITY` says the connection carries no RDP encryption
//! of its own — TLS is underneath it already — and `CS_NET` names the static virtual
//! channels. Nothing asks for a message channel, a multitransport tunnel or a monitor
//! layout, so the server has nothing to answer about them.

use super::wire::{Malformed, Reader, Writer};
use super::{channel, per};

/// `ConnectData` up to the connectPDU length, which never varies: a CHOICE selecting
/// the object-identifier form of the key, and the identifier itself — T.124 (02/98),
/// `{ itu-t recommendation t 124 version(0) 1 }` — PER-encoded as a length and six
/// tuple bytes, the first two packed into one.
const CONNECT_DATA: [u8; 7] = [0x00, 0x05, 0x00, 0x14, 0x7C, 0x00, 0x01];

/// The H.221 key that marks a user data block as RDP's rather than some other T.124
/// application's. "Duca" goes out, inside [`CREATE_REQUEST`]; "McDn" comes back.
const SERVER_KEY: &[u8; 4] = b"McDn";

/// `ConferenceCreateRequest` from its CHOICE index to the end of the H.221 key: every
/// byte of it is the same on every RDP connection, so it is written as the constant it
/// is rather than assembled from a schema nothing else uses.
///
/// In order: the CHOICE selecting `conferenceCreateRequest`, the OPTIONAL bitmap
/// selecting `userData` alone, `conferenceName` as the numeric string "1" (PER packs
/// two digits to a byte, padding the second with zero, and the string's length is
/// omitted because its lower bound is its length), a byte of alignment padding, a SET
/// OF count of one, the CHOICE selecting `h221NonStandard` with its value present, and
/// the key.
const CREATE_REQUEST: [u8; 12] =
    [0x00, 0x08, 0x00, 0x10, 0x00, 0x01, 0xC0, 0x00, b'D', b'u', b'c', b'a'];

/// Client data block types.
const CS_CORE: u16 = 0xC001;
const CS_SECURITY: u16 = 0xC002;
const CS_NET: u16 = 0xC003;

/// Server data block types. The ones this client does not ask for are still named,
/// because a server may send them anyway and the walk has to step over them.
const SC_CORE: u16 = 0x0C01;
const SC_SECURITY: u16 = 0x0C02;
const SC_NET: u16 = 0x0C03;

/// A user data block's own header: type and length, the length counting itself.
const BLOCK_HEADER: usize = 4;

/// The RDP version this client claims.
///
/// 5.0 and later, which is every server that has ever spoken TLS. Claiming a newer one
/// buys nothing: the later versions gate features this client does not use, and each
/// of them is a PDU the server would then be entitled to send.
const RDP_VERSION_5_PLUS: u32 = 0x0008_0004;

/// `highColorDepth`: 24 bits per pixel. 32 is asked for separately, by the pair of
/// flags below — the field itself has no value for it.
const HIGH_COLOR_24BPP: u16 = 0x0018;

/// `supportedColorDepths`: 32, 24, 16 and 15 bits per pixel.
const COLOR_DEPTHS: u16 = 0x000F;

/// `earlyCapabilityFlags`, and each one is a promise:
///
/// - `0x0001` this client understands the Set Error Info PDU, which is how a server
///   says *why* it is closing a connection instead of just closing it.
/// - `0x0002` it wants a 32-bit session.
/// - `0x0008` it accepts a server key longer than 512 bits.
/// - `0x0020` `connectionType` below is a real value rather than a zero.
///
/// Nothing else is claimed. Every other flag turns on a PDU the server may then send
/// — status info, monitor layout, heartbeats — and a PDU this client does not handle
/// is a connection that ends badly later rather than a feature.
const EARLY_CAPABILITIES: u16 = 0x0001 | 0x0002 | 0x0008 | 0x0020;

/// `RNS_UD_CS_SUPPORT_DYNVC_GFX_PROTOCOL`: this client takes the graphics pipeline
/// (MS-RDPEGFX), so the server may open the Graphics dynamic channel and draw the
/// desktop there instead of with bitmap updates. Claimed only when the session
/// asked for it — see [`ConferenceCreateRequest::graphics`].
const SUPPORT_DYNVC_GFX_PROTOCOL: u16 = 0x0100;

/// `connectionType`: LAN. It tunes nothing on the wire; it tells the server which
/// visual effects the user is likely to tolerate.
const CONNECTION_TYPE_LAN: u8 = 0x06;

/// `colorDepth` and `postBeta2ColorDepth`, both superseded by `highColorDepth` and
/// both still required to be a legal value: 8 bits per pixel.
const RNS_UD_COLOR_8BPP: u16 = 0xCA01;

/// `SASSequence`: Ctrl+Alt+Del, the only value the field has ever had.
const RNS_UD_SAS_DEL: u16 = 0xAA03;

/// `CHANNEL_OPTION_SHOW_PROTOCOL`, which every client sets on the clipboard and on
/// nothing else. The server is told here, and reminded on every chunk — see
/// [`Channel::chunk_flags`].
const SHOW_PROTOCOL: u32 = 0x0020_0000;

/// A static virtual channel, asked for by name in `CS_NET` and given a number by the
/// server in `SC_NET`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channel {
    /// Seven bytes at most, ASCII: the field is eight with a terminator.
    pub name: &'static str,
    pub options: u32,
}

impl Channel {
    /// The dynamic virtual channel transport, which carries Display Control — how a
    /// client tells the server the window has been resized.
    pub const DYNAMIC: Self = Self {
        name: "drdynvc",
        // Initialized, and compressed and encrypted by the server at its discretion.
        // The two security bits are historical: under TLS the server ignores them.
        options: 0x8000_0000 | 0x4000_0000 | 0x0080_0000,
    };

    /// The clipboard, which is a static channel of its own rather than a dynamic one:
    /// MS-RDPECLIP predates that transport, and a server opens this channel because
    /// the client named it here.
    pub const CLIPBOARD: Self = Self {
        name: "cliprdr",
        // The same three as above, and the one that is not decoration: without
        // `CHANNEL_OPTION_SHOW_PROTOCOL` — and the matching flag on every chunk — a
        // Windows host answers nothing at all on this channel. See
        // [`Channel::chunk_flags`].
        options: 0x8000_0000 | 0x4000_0000 | 0x0080_0000 | SHOW_PROTOCOL,
    };

    /// What every chunk of this channel's own PDUs wears beyond the first and last
    /// markers: [`channel::SHOW_PROTOCOL`] for a channel whose options asked for it,
    /// and nothing for one that did not.
    ///
    /// Derived from the options rather than written twice, because the two must agree:
    /// a Windows host reads nothing off a chunk whose header disagrees with what the
    /// channel was opened as — in *either* direction of disagreement. See
    /// [`channel::SHOW_PROTOCOL`], which records what each mistake looks like.
    pub fn chunk_flags(self) -> u32 {
        match self.options & SHOW_PROTOCOL != 0 {
            true => channel::SHOW_PROTOCOL,
            false => 0,
        }
    }

    /// The eight bytes the name occupies on the wire.
    fn wire_name(&self) -> [u8; 8] {
        let mut bytes = [0_u8; 8];
        let name = self.name.as_bytes();
        assert!(name.len() < bytes.len(), "a channel name is seven bytes and a terminator");
        bytes[..name.len()].copy_from_slice(name);
        bytes
    }
}

/// What the client asks for, and everything the server needs before a session exists.
pub struct ConferenceCreateRequest<'a> {
    pub width: u16,
    pub height: u16,
    /// The gateway's own host name, as the server will show it in session lists.
    pub client_name: &'a str,
    /// A Windows keyboard layout identifier — `0x0409` for US English.
    pub keyboard_layout: u32,
    /// What the X.224 negotiation settled on. The server compares it with what it
    /// answered, so a client cannot be talked down to a weaker protocol by a machine
    /// in the middle rewriting the negotiation.
    pub selected_protocol: u32,
    pub channels: &'a [Channel],
    /// Whether to offer the graphics pipeline. A server that is offered it opens the
    /// Graphics channel over [`Channel::DYNAMIC`], which `channels` must then name.
    pub graphics: bool,
}

impl ConferenceCreateRequest<'_> {
    /// The whole `ConnectData`, ready to be the `userData` of an MCS Connect-Initial.
    pub fn encode(&self) -> Vec<u8> {
        let blocks = self.blocks();
        let blocks_length =
            u16::try_from(blocks.len()).expect("the client data blocks are a few hundred bytes");
        // `connectPDU` measures everything after its own length: the constant prefix,
        // the length of the user data, and the user data.
        let connect_pdu = u16::try_from(CREATE_REQUEST.len())
            .expect("a 12-byte constant")
            .saturating_add(u16::try_from(per::length_size(blocks_length)).expect("1 or 2"))
            .saturating_add(blocks_length);

        let mut w = Writer::with_capacity(CONNECT_DATA.len() + 3 + usize::from(connect_pdu));
        w.bytes(&CONNECT_DATA);
        per::write_length(&mut w, connect_pdu);
        w.bytes(&CREATE_REQUEST);
        per::write_octet_string(&mut w, &blocks, 0);
        w.finish()
    }

    /// The RDP blocks, which is what the server actually reads.
    fn blocks(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(256 + self.channels.len() * 12);
        self.core(&mut w);
        security(&mut w);
        self.network(&mut w);
        w.finish()
    }

    fn core(&self, w: &mut Writer) {
        block_header(w, CS_CORE, 230);
        w.u32_le(RDP_VERSION_5_PLUS);
        w.u16_le(self.width);
        w.u16_le(self.height);
        w.u16_le(RNS_UD_COLOR_8BPP); // colorDepth, superseded by highColorDepth
        w.u16_le(RNS_UD_SAS_DEL);
        w.u32_le(self.keyboard_layout);
        w.u32_le(0); // clientBuild
        w.bytes(&utf16_fixed::<32>(self.client_name));
        w.u32_le(4); // keyboardType: IBM enhanced, 101 or 102 keys
        w.u32_le(0); // keyboardSubType
        w.u32_le(12); // keyboardFunctionKey
        w.zeros(64); // imeFileName
        w.u16_le(RNS_UD_COLOR_8BPP); // postBeta2ColorDepth, superseded as above
        w.u16_le(1); // clientProductId, which has only ever been 1
        w.u32_le(0); // serialNumber
        w.u16_le(HIGH_COLOR_24BPP);
        w.u16_le(COLOR_DEPTHS);
        let graphics = if self.graphics { SUPPORT_DYNVC_GFX_PROTOCOL } else { 0 };
        w.u16_le(EARLY_CAPABILITIES | graphics);
        w.zeros(64); // clientDigProductId: an installation identifier we do not have
        w.u8(CONNECTION_TYPE_LAN);
        w.u8(0); // pad1octet
        w.u32_le(self.selected_protocol);
        // The physical size and scale of the client's screen, which would let the
        // server pick a DPI. Zero means it has not been measured, and the gateway
        // does not measure it: a browser reports density, not millimetres.
        w.u32_le(0); // desktopPhysicalWidth
        w.u32_le(0); // desktopPhysicalHeight
        w.u16_le(0); // desktopOrientation
        w.u32_le(0); // desktopScaleFactor
        w.u32_le(0); // deviceScaleFactor
    }

    fn network(&self, w: &mut Writer) {
        if self.channels.is_empty() {
            return;
        }
        let length = BLOCK_HEADER + 4 + self.channels.len() * 12;
        block_header(w, CS_NET, u16::try_from(length - BLOCK_HEADER).expect("a few channels"));
        w.u32_le(u32::try_from(self.channels.len()).expect("a few channels"));
        for channel in self.channels {
            w.bytes(&channel.wire_name());
            w.u32_le(channel.options);
        }
    }
}

/// `CS_SECURITY`, which on this client says the same thing twice: no encryption.
///
/// The RDP security layer is what a connection uses when there is no TLS underneath
/// it. There always is one here, so both method fields are zero — and a server that
/// answered with a method anyway would be asking for an encryption this client does
/// not implement, which [`ConferenceCreateResponse::decode`] refuses.
fn security(w: &mut Writer) {
    block_header(w, CS_SECURITY, 8);
    w.u32_le(0); // encryptionMethods
    w.u32_le(0); // extEncryptionMethods
}

fn block_header(w: &mut Writer, block: u16, contents: u16) {
    w.u16_le(block);
    w.u16_le(contents + u16::try_from(BLOCK_HEADER).expect("4"));
}

/// `str` as a fixed-width, NUL-terminated UTF-16 field.
///
/// Truncated to fit, on a code unit that can stand alone: cutting a surrogate pair in
/// half would put a lone half on the wire, which is not text in any encoding.
fn utf16_fixed<const N: usize>(text: &str) -> [u8; N] {
    let mut bytes = [0_u8; N];
    let mut at = 0;
    // By character rather than by code unit, so a character that takes two units is
    // written whole or not at all. Two more bytes are always kept back for the
    // terminator.
    for character in text.chars() {
        let width = character.len_utf16() * 2;
        if at + width + 2 > N {
            break;
        }
        let mut units = [0_u16; 2];
        for unit in character.encode_utf16(&mut units) {
            bytes[at..at + 2].copy_from_slice(&unit.to_le_bytes());
            at += 2;
        }
    }
    bytes
}

/// What the server answers: the numbers every later PDU is addressed with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConferenceCreateResponse {
    /// The channel every PDU that is not for a virtual channel travels on — the
    /// desktop, the input, the licensing and the capability exchange. Conventionally
    /// 1003, but the server names it and this client uses what it names.
    pub io_channel: u16,
    /// The channels asked for in `CS_NET`, numbered by the server, in the order they
    /// were asked for.
    pub channels: Vec<u16>,
}

impl ConferenceCreateResponse {
    /// Decode the `userData` of an MCS Connect-Response.
    pub fn decode(bytes: &[u8]) -> Result<Self, Malformed> {
        const WHAT: &str = "a GCC Conference Create Response";
        let mut r = Reader::new(WHAT, bytes);

        // ConnectData, whose key must be T.124's, or these are somebody else's blocks.
        let key = r.bytes(CONNECT_DATA.len())?;
        if key != CONNECT_DATA {
            return Err(r.refuse("a T.124 object identifier starting", key[0]));
        }
        per::read_length(&mut r)?; // connectPDU, which the fields below re-measure
        r.u8()?; // CHOICE: conferenceCreateResponse
        per::read_integer16(&mut r, 1001)?; // nodeID, which nothing addresses
        per::read_integer(&mut r)?; // tag
        let result = r.u8()?; // ENUMERATED result
        if result != 0 {
            return Err(r.refuse("a refusal to create the conference", result));
        }
        r.u8()?; // SET OF count, which RDP always makes one
        r.u8()?; // CHOICE: h221NonStandard, value present
        per::expect_octet_string(&mut r, "an H.221 key starting", SERVER_KEY)?;

        let blocks = per::read_octet_string(&mut r, 0)?;
        Self::blocks(blocks)
    }

    fn blocks(bytes: &[u8]) -> Result<Self, Malformed> {
        const WHAT: &str = "a GCC server data block";
        let mut r = Reader::new(WHAT, bytes);
        let mut network = None;

        while !r.is_empty() {
            let block = r.u16_le()?;
            let length = usize::from(r.u16_le()?);
            let size = length
                .checked_sub(BLOCK_HEADER)
                .ok_or_else(|| r.refuse("a block length of", u64::try_from(length).unwrap_or(0)))?;
            let contents = Reader::new(WHAT, r.bytes(size)?);
            match block {
                // The server's own version and capability flags. Nothing here reads
                // them: this client claims the oldest version it can and asks for
                // nothing a server has to opt into.
                SC_CORE => {}
                SC_SECURITY => Self::security(contents)?,
                SC_NET => network = Some(Self::network(contents)?),
                // A block for something this client did not ask for. The server sends
                // them unprompted — a multitransport tunnel offer, for instance — and
                // the connection is no worse for stepping over one.
                _ => {}
            }
        }

        network.ok_or(Malformed::Missing { what: WHAT, field: "the channel numbers of SC_NET" })
    }

    /// `SC_SECURITY`, which must agree that there is no RDP encryption.
    ///
    /// A method or a level here means the server wants the RDP security layer, which
    /// this client does not implement and would not use under TLS if it did. Refusing
    /// is the honest answer: going on would send a logon PDU in the clear inside a
    /// session the server believes is encrypted.
    fn security(mut r: Reader<'_>) -> Result<(), Malformed> {
        let method = r.u32_le()?;
        let level = r.u32_le()?;
        if method != 0 {
            return Err(r.refuse("an RDP encryption method", method));
        }
        if level != 0 {
            return Err(r.refuse("an RDP encryption level", level));
        }
        Ok(())
    }

    fn network(mut r: Reader<'_>) -> Result<Self, Malformed> {
        let io_channel = r.u16_le()?;
        let count = usize::from(r.u16_le()?);
        let mut channels = Vec::with_capacity(count);
        for _ in 0..count {
            channels.push(r.u16_le()?);
        }
        Ok(Self { io_channel, channels })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ConferenceCreateRequest<'static> {
        const CHANNELS: [Channel; 1] = [Channel::DYNAMIC];
        ConferenceCreateRequest {
            width: 1920,
            height: 1080,
            client_name: "gateway",
            keyboard_layout: 0x0409,
            selected_protocol: 2,
            channels: &CHANNELS,
            graphics: false,
        }
    }

    /// The offset of `earlyCapabilityFlags` inside `CS_CORE`'s contents, after the
    /// version, the size, the depths, the layout, the build, the name, the keyboard
    /// fields, the IME name, the two superseded depths, the product id, the serial
    /// number, the high colour depth and the supported depths.
    const EARLY_CAPABILITIES_AT: usize = 4 + 2 + 2 + 2 + 2 + 4 + 4 + 32 + 4 + 4 + 4 + 64 + 2 + 2 + 4 + 2 + 2;

    fn early_capabilities(request: &ConferenceCreateRequest<'_>) -> u16 {
        let bytes = request.blocks();
        // The first block is CS_CORE, whose header is four bytes.
        let at = BLOCK_HEADER + EARLY_CAPABILITIES_AT;
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    /// The one bit that lets a Windows host open the Graphics channel, present when
    /// the pipeline is wanted and absent — leaving the flags exactly as they were —
    /// when it is not.
    #[test]
    fn the_graphics_pipeline_is_offered_by_one_early_capability_bit() {
        let without = request();
        assert_eq!(early_capabilities(&without), EARLY_CAPABILITIES);
        let with = ConferenceCreateRequest { graphics: true, ..request() };
        assert_eq!(early_capabilities(&with), EARLY_CAPABILITIES | 0x0100);
    }

    #[test]
    fn the_envelope_announces_exactly_what_follows_it() {
        let bytes = request().encode();
        let mut r = Reader::new("a test", &bytes);
        assert_eq!(r.bytes(CONNECT_DATA.len()).unwrap(), CONNECT_DATA);
        let connect_pdu = usize::from(per::read_length(&mut r).unwrap());
        // The length counts every byte after itself.
        assert_eq!(connect_pdu, bytes.len() - r.at());
        assert_eq!(r.bytes(CREATE_REQUEST.len()).unwrap(), CREATE_REQUEST);
        let blocks = per::read_octet_string(&mut r, 0).unwrap();
        assert!(r.is_empty());
        // CS_CORE, CS_SECURITY, and CS_NET with one channel.
        assert_eq!(blocks.len(), 234 + 12 + 20);
    }

    #[test]
    fn every_block_announces_its_own_length() {
        let bytes = request().encode();
        let mut r = Reader::new("a test", &bytes);
        r.skip(CONNECT_DATA.len()).unwrap();
        per::read_length(&mut r).unwrap();
        r.skip(CREATE_REQUEST.len()).unwrap();
        let blocks = per::read_octet_string(&mut r, 0).unwrap();

        let mut r = Reader::new("a test", blocks);
        let mut seen = Vec::new();
        while !r.is_empty() {
            let block = r.u16_le().unwrap();
            let length = usize::from(r.u16_le().unwrap());
            r.skip(length - BLOCK_HEADER).unwrap();
            seen.push((block, length));
        }
        assert_eq!(seen, vec![(CS_CORE, 234), (CS_SECURITY, 12), (CS_NET, 20)]);
    }

    /// The one field in `CS_CORE` that is neither constant nor copied straight from a
    /// setting: a name is text, and the wire wants a fixed-width UTF-16 field.
    #[test]
    fn a_client_name_is_terminated_and_padded_and_never_cut_in_half() {
        assert_eq!(&utf16_fixed::<8>("ab"), b"a\0b\0\0\0\0\0");
        // Fifteen units and a terminator is all that fits.
        let bytes = utf16_fixed::<32>(&"x".repeat(40));
        assert_eq!(bytes[..30], [b'x', 0].repeat(15)[..]);
        assert_eq!(bytes[30..], [0, 0]);
        // A character outside the BMP is two code units, and goes on whole or not at
        // all: half a surrogate pair is not text in any encoding.
        assert_eq!(&utf16_fixed::<8>("\u{1F600}x"), &[0x3D, 0xD8, 0x00, 0xDE, b'x', 0, 0, 0]);
        assert_eq!(&utf16_fixed::<6>("\u{1F600}x"), &[0x3D, 0xD8, 0x00, 0xDE, 0, 0]);
        assert_eq!(&utf16_fixed::<4>("\u{1F600}x"), &[0, 0, 0, 0]);
    }

    /// The server's answer, as a Windows host writes it: a core block, a security
    /// block saying there is none, and the channel numbers.
    fn response(channels: &[u16], security: (u32, u32)) -> Vec<u8> {
        let mut blocks = Writer::new();
        blocks.u16_le(SC_CORE);
        blocks.u16_le(8);
        blocks.u32_le(0x0008_0004);
        blocks.u16_le(SC_SECURITY);
        blocks.u16_le(12);
        blocks.u32_le(security.0);
        blocks.u32_le(security.1);
        blocks.u16_le(SC_NET);
        // An odd number of channels is padded to a four-byte boundary, inside the
        // block's own length — which is how a real server writes it.
        let padding = (channels.len() % 2) * 2;
        blocks.u16_le(u16::try_from(8 + channels.len() * 2 + padding).unwrap());
        blocks.u16_le(1003);
        blocks.u16_le(u16::try_from(channels.len()).unwrap());
        for channel in channels {
            blocks.u16_le(*channel);
        }
        blocks.zeros(padding);
        let blocks = blocks.finish();

        let mut w = Writer::new();
        w.bytes(&CONNECT_DATA);
        per::write_length(&mut w, u16::try_from(blocks.len() + 20).unwrap());
        w.u8(0x14); // CHOICE: conferenceCreateResponse
        per::write_integer16(&mut w, 1002, 1001); // nodeID
        per::write_integer(&mut w, 1); // tag
        w.u8(0); // result: success
        w.u8(1); // one set of user data
        w.u8(0xC0); // CHOICE: h221NonStandard, value present
        per::write_octet_string(&mut w, SERVER_KEY, 4);
        per::write_octet_string(&mut w, &blocks, 0);
        w.finish()
    }

    #[test]
    fn a_response_yields_the_numbers_every_later_pdu_is_addressed_with() {
        let decoded = ConferenceCreateResponse::decode(&response(&[1004, 1005], (0, 0))).unwrap();
        assert_eq!(decoded, ConferenceCreateResponse {
            io_channel: 1003,
            channels: vec![1004, 1005]
        });
    }

    /// A server asking for the RDP security layer is asking for something this client
    /// does not do, and the connection has to end there rather than at the logon PDU.
    #[test]
    fn an_encrypted_session_is_refused_by_name() {
        let err = ConferenceCreateResponse::decode(&response(&[1004], (2, 0))).unwrap_err();
        assert_eq!(
            err.to_string(),
            "a GCC server data block carries an RDP encryption method 0x2, which this client does \
             not accept"
        );
        let err = ConferenceCreateResponse::decode(&response(&[1004], (0, 1))).unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "an RDP encryption level", .. }));
    }

    #[test]
    fn an_answer_with_no_channel_block_is_an_error_rather_than_an_empty_list() {
        let mut bytes = response(&[1004], (0, 0));
        // Turn SC_NET into a block type nothing reads.
        let at = bytes.windows(2).position(|w| w == SC_NET.to_le_bytes()).unwrap();
        bytes[at..at + 2].copy_from_slice(&0x0C09_u16.to_le_bytes());
        assert!(ConferenceCreateResponse::decode(&bytes).is_err());
    }
}
