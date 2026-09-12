//! The gateway's own RDP code, against a real Windows host.
//!
//! [`remotex::rdp_client::proto`] is tested against the specification by its own unit
//! tests. This asks the other question — whether Windows agrees — by driving the
//! connection sequence over a real socket with nothing else in the process: our bytes
//! out, the host's bytes in, our decoding of the answer.
//!
//! There is no container here and no second kind of server. The target is a current
//! Windows host, so the probe borrows one from the operator's `tmp/test_uat.toml`,
//! named by [`TARGET_ENV`]:
//!
//! ```sh
//! REMOTEX_UAT_TARGET=windows-ent-sandbox \
//!   cargo test --test rdp_proto_probe -- --ignored --nocapture
//! ```
//!
//! It goes as far as the new stack can carry a connection, and then watches the
//! desktop paint itself. What it proves at each step is the step a server is the only
//! judge of: that the host accepts what we sent, and that what it sends back decodes.
//!
//! The pixels get a second judge. Every compressed rectangle is decoded twice — once
//! by [`remotex::rdp_client::proto::planar`] and once by IronRDP, which is the stack
//! this one is replacing and which has been rendering this host correctly all along —
//! and the two are compared. Plane order and row order are the two mistakes a planar
//! decoder makes that produce a picture which is merely *wrong* rather than one that
//! fails to decode, and neither is visible in a hex dump.
//!
//! What it decodes is also written out, as `tmp/rdp_proto_probe.ppm`, for an operator
//! who would rather look at the desktop than read a coverage figure.

mod common;

use std::time::Duration;

use ironrdp::core::{Decode as _, ReadCursor, encode_vec};
use ironrdp::graphics::pointer::{DecodedPointer, PointerBitmapTarget};
use ironrdp::graphics::rdp6::BitmapStreamDecoder;
use ironrdp::pdu::input::fast_path::{FastPathInput, FastPathInputEvent, KeyboardFlags};
use ironrdp::pdu::input::mouse::PointerFlags;
use ironrdp::pdu::input::mouse_x::PointerXFlags;
use ironrdp::pdu::input::{MousePdu, MouseXPdu};
use ironrdp::pdu::pointer::{ColorPointerAttribute, LargePointerAttribute, PointerAttribute};
use remotex::rdp_client::proto::bitmap::{self, Scratch};
use remotex::rdp_client::proto::capabilities::{ConfirmActive, DemandActive};
use remotex::rdp_client::proto::credssp::{self, Credentials};
use remotex::rdp_client::proto::fastpath::{self, Fragments};
use remotex::rdp_client::proto::finalization::{self, Response};
use remotex::rdp_client::proto::gcc::{Channel, ConferenceCreateRequest, ConferenceCreateResponse};
use remotex::rdp_client::proto::input::{self, Button, Event};
use remotex::rdp_client::proto::pointer::{self, Pointer, Shape};
use remotex::rdp_client::proto::info::ClientInfo;
use remotex::rdp_client::proto::share::{self, Pdu};
use remotex::rdp_client::proto::{license, mcs, tls};
use remotex::rdp_client::proto::x224::{
    ConfirmFlags, ConnectionConfirm, ConnectionRequest, Security, TPKT_HEADER, frame_length,
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// Which target in `tmp/test_uat.toml` to dial — see the module docs.
const TARGET_ENV: &str = "REMOTEX_UAT_TARGET";

/// How long the whole sequence gets. Generous: a Windows host that is asleep takes
/// its time over the first packet.
const BUDGET: Duration = Duration::from_secs(30);

/// Network Level Authentication, and nothing else. See the module docs on
/// `rdp_client::proto`.
const OFFERED: Security = Security::HYBRID;

/// The static virtual channels this client asks for. One, and it is the transport the
/// Display Control resize channel later rides on.
const CHANNELS: [Channel; 1] = [Channel::DYNAMIC];

/// A size to open at. Nothing in this probe depends on it; the server just has to
/// accept it.
const DESKTOP: (u16, u16) = (1920, 1080);

/// US English, which every Windows host has.
const KEYBOARD_LAYOUT: u32 = 0x0409;

/// How long to watch the desktop paint. A Windows host repaints the whole screen as
/// soon as the share is live, so this is long enough to see every rectangle of it and
/// short enough that the probe is not a wait.
const WATCH: Duration = Duration::from_secs(5);

/// Where the decoded desktop is left for an operator to look at.
const PICTURE: &str = "tmp/rdp_proto_probe.ppm";

#[tokio::test]
#[ignore = "requires a real Windows host from tmp/test_uat.toml"]
async fn a_windows_host_hands_over_a_live_desktop_to_our_connection_sequence() {
    common::init_logging();
    let name = std::env::var(TARGET_ENV).unwrap_or_else(|_| {
        panic!("set {TARGET_ENV} to the name of an rdp target in tmp/test_uat.toml")
    });
    let target = common::uat_target(&name);
    // The name TLS and CredSSP are told, without the brackets an IPv6 literal is
    // written with in a host:port.
    let server_name = target.host.trim_start_matches('[').trim_end_matches(']').to_owned();
    println!("rdp_proto_probe: {name} ({server_name}:{})", target.port);

    tokio::time::timeout(BUDGET, async {
        let mut tcp = TcpStream::connect((server_name.as_str(), target.port))
            .await
            .expect("connect to the host");
        // This end of the socket, which the Client Info PDU tells the server about.
        let client_address = tcp.local_addr().expect("the local address of the socket").ip();

        // 1. The X.224 negotiation, in the clear.
        let request =
            ConnectionRequest { cookie: Some(target.username.clone()), protocols: OFFERED };
        let bytes = request.encode();
        println!("-> {} bytes: {}", bytes.len(), hex(&bytes));
        tcp.write_all(&bytes).await.expect("send the connection request");

        let frame = read_frame(&mut tcp).await;
        println!("<- {} bytes: {}", frame.len(), hex(&frame));
        let confirm = ConnectionConfirm::decode(&frame).expect("an X.224 Connection Confirm");
        let ConnectionConfirm::Negotiated { protocol, flags } = confirm else {
            panic!("the host did not negotiate: {confirm:?}");
        };
        println!(
            "negotiated {protocol:?}, flags {:#04x} (extended client data: {})",
            flags.bits(),
            flags.contains(ConfirmFlags::EXTENDED_CLIENT_DATA)
        );
        assert_eq!(protocol, Security::HYBRID, "only NLA is offered, so only NLA can be chosen");

        // 2. The TLS session everything after this lives inside.
        let mut stream = tls::upgrade(tcp, &server_name).await.expect("the TLS handshake");
        let public_key = tls::public_key(&stream).expect("the server's public key");
        println!("TLS up, server public key {} bytes", public_key.len());

        // 3. CredSSP. The host is the only thing that can say whether the NTLM
        //    exchange and the public-key binding were right.
        let credentials = Credentials {
            username: &target.username,
            password: &target.password,
            domain: target.domain.as_deref(),
        };
        credssp::authenticate(&mut stream, &server_name, &credentials, public_key)
            .await
            .expect("the host to accept the credentials");
        println!("authenticated as {}", target.username);

        // 4. MCS Connect-Initial, carrying the GCC conference. The answer is the one
        //    that matters: it numbers every channel the session will use.
        let conference = ConferenceCreateRequest {
            width: DESKTOP.0,
            height: DESKTOP.1,
            client_name: "remotex",
            keyboard_layout: KEYBOARD_LAYOUT,
            selected_protocol: protocol.bits(),
            channels: &CHANNELS,
        }
        .encode();
        let initial = mcs::connect_initial(&conference).expect("the conference fits a frame");
        println!("-> MCS Connect-Initial, {} bytes", initial.len());
        stream.write_all(&initial).await.expect("send the Connect-Initial");
        stream.flush().await.expect("send the Connect-Initial");

        let frame = read_frame(&mut stream).await;
        println!("<- MCS Connect-Response, {} bytes", frame.len());
        let answer = mcs::connect_response(&frame).expect("an MCS Connect-Response");
        let ConferenceCreateResponse { io_channel, channels } =
            ConferenceCreateResponse::decode(answer).expect("a GCC Conference Create Response");
        println!("I/O channel {io_channel}, virtual channels {channels:?}");
        assert_eq!(
            channels.len(),
            CHANNELS.len(),
            "the server numbers exactly the channels that were asked for"
        );

        // 5. Erect the domain — unanswered — and attach a user to it.
        stream.write_all(&mcs::erect_domain_request()).await.expect("send the Erect Domain");
        stream.write_all(&mcs::attach_user_request()).await.expect("send the Attach User");
        stream.flush().await.expect("send the Attach User");

        let frame = read_frame(&mut stream).await;
        let user = mcs::attach_user_confirm(&frame).expect("an MCS Attach User Confirm");
        println!("attached as user {user}");

        // 6. Join every channel, the user's own first. Each is its own round trip,
        //    and the server may answer with a different number than was asked for.
        for channel in std::iter::once(user).chain(std::iter::once(io_channel)).chain(channels) {
            stream
                .write_all(&mcs::channel_join_request(user, channel))
                .await
                .expect("send a Channel Join Request");
            stream.flush().await.expect("send a Channel Join Request");
            let frame = read_frame(&mut stream).await;
            let joined = mcs::channel_join_confirm(&frame).expect("an MCS Channel Join Confirm");
            println!("joined channel {joined}");
            assert_eq!(joined, channel, "the server joined the channel that was asked for");
        }

        // 7. The logon. The credentials went through CredSSP already; this says to
        //    use them rather than to show a logon screen.
        let logon = ClientInfo {
            username: &target.username,
            password: &target.password,
            domain: target.domain.as_deref(),
            address: client_address,
        }
        .encode()
        .expect("the Client Info PDU fits a frame");
        send(&mut stream, user, io_channel, &logon).await;
        println!("-> Client Info, {} bytes", logon.len());

        // 8. Licensing, which on a host like this one is a single PDU saying there is
        //    none.
        let payload = receive(&mut stream, io_channel).await;
        license::accept(&payload).expect("the host to say no licence is needed");
        println!("<- licensing: no licence needed");

        // 9. The capability exchange. The server's Demand Active is the first PDU
        //    that says what the session will actually be.
        let payload = receive(&mut stream, io_channel).await;
        let Pdu::DemandActive(body) = share::decode(&payload).expect("a share control PDU") else {
            panic!("the server did not demand a share once licensing was done");
        };
        let demand = DemandActive::decode(body).expect("an RDP Demand Active PDU");
        println!(
            "<- Demand Active: share {:#x}, desktop {}x{}, fragments up to {} bytes",
            demand.share_id, demand.width, demand.height, demand.multifragment
        );
        assert_eq!(
            (demand.width, demand.height),
            DESKTOP,
            "the host opened the desktop that was asked for"
        );

        let confirm = ConfirmActive {
            share_id: demand.share_id,
            width: demand.width,
            height: demand.height,
            keyboard_layout: KEYBOARD_LAYOUT,
            multifragment: demand.multifragment,
        }
        .encode();
        let confirm = share::confirm_active(user, &confirm);
        send(&mut stream, user, io_channel, &confirm).await;
        println!("-> Confirm Active, {} bytes", confirm.len());

        // 10. The finalization handshake. All four go out without waiting; the
        //     server's four come back in its own time, with session PDUs among them.
        for request in finalization::requests(user, demand.share_id) {
            stream.write_all(&mcs::send_data_request(user, io_channel, &request).unwrap())
                .await
                .expect("send a finalization PDU");
        }
        stream.flush().await.expect("send the finalization PDUs");
        println!("-> synchronize, cooperate, request control, font list");

        loop {
            let payload = receive(&mut stream, io_channel).await;
            let Pdu::Data(data) = share::decode(&payload).expect("a share control PDU") else {
                panic!("the server deactivated the share during finalization");
            };
            let response = finalization::response(&data).expect("a finalization PDU");
            println!("<- {response:?}");
            if response == Response::FontMap {
                break;
            }
        }
        println!("the desktop is live");

        // 11. The updates. Nothing is asked for: a share that has just gone live
        //     paints itself, and what arrives is whatever the host decided to send.
        watch(&mut stream, &demand, io_channel).await;
    })
    .await
    .expect("the connection sequence finished within its budget");
}

/// One PDU out, addressed to a channel.
async fn send(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    user: u16,
    channel: u16,
    pdu: &[u8],
) {
    let frame = mcs::send_data_request(user, channel, pdu).expect("the PDU fits one Send Data");
    stream.write_all(&frame).await.expect("send a PDU");
    stream.flush().await.expect("send a PDU");
}

/// One PDU in, off the channel it was expected on.
async fn receive(stream: &mut (impl AsyncRead + AsyncWrite + Unpin), channel: u16) -> Vec<u8> {
    let frame = read_frame(stream).await;
    let data = mcs::send_data_indication(&frame).expect("an MCS Send Data Indication");
    assert_eq!(data.channel, channel, "the PDU arrived on the channel it was expected on");
    data.payload.to_vec()
}

/// One whole TPKT frame: the header, then exactly the length it announces.
async fn read_frame(stream: &mut (impl AsyncRead + AsyncWrite + Unpin)) -> Vec<u8> {
    let mut header = [0_u8; TPKT_HEADER];
    stream.read_exact(&mut header).await.expect("read a TPKT header");
    let length = frame_length(&header).expect("the answer is a TPKT frame");
    let mut frame = vec![0_u8; length];
    frame[..TPKT_HEADER].copy_from_slice(&header);
    stream.read_exact(&mut frame[TPKT_HEADER..]).await.expect("read the rest of the frame");
    frame
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// Watch the desktop paint itself, decoding everything that arrives.
///
/// Nothing is sent from here. What a share sends when it goes live is the host's
/// decision, and the point is to find out what that is and whether this client can
/// read it — not to provoke a particular update.
async fn watch(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    demand: &DemandActive,
    io_channel: u16,
) {
    let (width, height) = (usize::from(demand.width), usize::from(demand.height));
    let mut desktop = vec![0_u8; width * height * 4];
    let mut painted = vec![false; width * height];

    let mut fragments = Fragments::new(demand.multifragment);
    let mut scratch = Scratch::default();
    let mut pixels = Vec::new();
    let mut ironrdp = BitmapStreamDecoder::default();
    let mut reference = Vec::new();
    let mut cursors = pointer::Cache::new();

    let mut seen: Vec<(&str, usize)> = Vec::new();
    let mut rectangles = 0_usize;
    let mut compressed = 0_usize;
    let mut bytes = 0_usize;

    let deadline = tokio::time::Instant::now() + WATCH;
    while let Ok(frame) = tokio::time::timeout_at(deadline, read_any(stream)).await {
        if !fastpath::is_output(frame[0]) {
            // The slow path still carries everything that is not an update. During a
            // quiet session that is a Set Error Info saying nothing is wrong.
            let data = mcs::send_data_indication(&frame).expect("an MCS Send Data Indication");
            if data.channel != io_channel {
                // The dynamic virtual channel, which the server opens as soon as the
                // share is live. Display Control rides on it, and reading it is the
                // phase after this one.
                println!("<- channel {}: {} bytes", data.channel, data.payload.len());
                continue;
            }
            // Named rather than printed: a Save Session Info PDU is a kilobyte of
            // Unicode, and what matters here is that it decoded and what it was.
            let what = match share::decode(data.payload).expect("a share control PDU") {
                Pdu::DemandActive(body) => format!("a Demand Active of {} bytes", body.len()),
                Pdu::DeactivateAll => "a Deactivate All".to_owned(),
                Pdu::Data(pdu) => {
                    format!("a data PDU of type {:#04x}, {} bytes", pdu.kind, pdu.body.len())
                }
            };
            println!("<- slow path: {what}");
            continue;
        }
        bytes += frame.len();
        for piece in fastpath::updates(&frame).expect("a fast-path output PDU") {
            let piece = piece.expect("a fast-path update");
            let Some(update) = fragments.push(piece).expect("a reassembled update") else {
                continue;
            };
            count(&mut seen, name(update.code));
            if is_pointer(update.code) {
                // Every pointer update is decoded, and every shape the server sends
                // fresh is decoded a second time by IronRDP and compared. A cursor is
                // small enough to print one line each: there are a dozen in a quiet
                // five seconds, and their depth is what decides this module's scope.
                let decoded = cursors.update(update.code, update.data).expect("a pointer update");
                match &decoded {
                    Pointer::Shape(shape) => {
                        println!("<- {}: xorBpp {}, {shape:?}", name(update.code), depth(update.code, update.data));
                        if update.code != fastpath::CACHED_POINTER {
                            check_pointer(update.code, update.data, shape);
                        }
                    }
                    other => println!("<- {}: {other:?}", name(update.code)),
                }
                continue;
            }
            if update.code != fastpath::BITMAP {
                continue;
            }
            for rectangle in bitmap::update(update.data).expect("a Bitmap Update") {
                rectangle.decode(&mut scratch, &mut pixels).expect("a rectangle of the desktop");
                assert_eq!(pixels.len(), rectangle.painted_bytes());
                if rectangle.compressed {
                    compressed += 1;
                    check(&mut ironrdp, &mut reference, &rectangle, &pixels);
                }
                blit(&mut desktop, &mut painted, width, &rectangle, &pixels);
                rectangles += 1;
            }
        }
    }

    seen.sort_unstable();
    println!("<- {bytes} bytes of fast-path output: {seen:?}");
    println!("   {rectangles} rectangles, {compressed} of them compressed");
    let covered = painted.iter().filter(|seen| **seen).count();
    println!("   {}% of the desktop painted", covered * 100 / painted.len());
    write_picture(&desktop, width, height);

    assert!(rectangles > 0, "a live share paints itself, and nothing arrived");
    assert!(covered > 0, "rectangles arrived and none of them landed on the desktop");
}

/// The pointer updates, which [`pointer::Cache`] is the whole reader of.
fn is_pointer(code: u8) -> bool {
    matches!(
        code,
        fastpath::POINTER_HIDDEN
            | fastpath::POINTER_DEFAULT
            | fastpath::POINTER_POSITION
            | fastpath::COLOR_POINTER
            | fastpath::CACHED_POINTER
            | fastpath::NEW_POINTER
            | fastpath::LARGE_POINTER
    )
}

/// The `xorBpp` an update carries, which a Colour Pointer Update does not: it is
/// always 24. A cached one carries no shape of its own at all.
fn depth(code: u8, body: &[u8]) -> u16 {
    match code {
        fastpath::COLOR_POINTER => 24,
        fastpath::CACHED_POINTER => 0,
        _ => u16::from_le_bytes([body[0], body[1]]),
    }
}

/// Decode the same shape with IronRDP, and insist the two agree.
///
/// The two masks are where a pointer decoder goes quietly wrong: the row order is
/// reversed for a colour shape and not for a monochrome one, the scanlines are padded,
/// and the andMask bit means two different things depending on the colour under it.
/// None of that shows up as a failure — it shows up as a cursor that looks nearly
/// right — so it is checked against the stack being replaced while that stack is still
/// here.
fn check_pointer(code: u8, body: &[u8], ours: &Shape) {
    let target = PointerBitmapTarget::Accelerated;
    let mut src = ReadCursor::new(body);
    let theirs = match code {
        fastpath::COLOR_POINTER => {
            let attribute = ColorPointerAttribute::decode(&mut src).expect("a colour pointer");
            DecodedPointer::decode_color_pointer_attribute(&attribute, target)
        }
        fastpath::NEW_POINTER => {
            let attribute = PointerAttribute::decode(&mut src).expect("a new pointer");
            DecodedPointer::decode_pointer_attribute(&attribute, target)
        }
        _ => {
            let attribute = LargePointerAttribute::decode(&mut src).expect("a large pointer");
            DecodedPointer::decode_large_pointer_attribute(&attribute, target)
        }
    }
    .expect("IronRDP decodes the shape too");
    assert_eq!(
        (ours.width, ours.height, ours.hotspot_x, ours.hotspot_y),
        (theirs.width, theirs.height, theirs.hotspot_x, theirs.hotspot_y),
        "the two decoders disagree about the shape's size"
    );
    assert_eq!(ours.rgba, theirs.bitmap_data, "the two decoders disagree about the pixels");
}

/// Decode the same rectangle with IronRDP, and insist the two agree.
///
/// IronRDP hands back the whole bitmap as `RGB24` in the order it was stored — bottom
/// row first — so the comparison applies the same turn and the same crop that
/// [`bitmap::Bitmap::decode`] does. Getting either of those wrong is the failure this
/// is here to catch.
fn check(
    ironrdp: &mut BitmapStreamDecoder,
    reference: &mut Vec<u8>,
    rectangle: &bitmap::Bitmap<'_>,
    ours: &[u8],
) {
    let (width, height) = (usize::from(rectangle.width), usize::from(rectangle.height));
    reference.clear();
    ironrdp
        .decode_bitmap_stream_to_rgb24(rectangle.data, reference, width, height)
        .expect("IronRDP to decode the same rectangle");

    let mut expected = Vec::with_capacity(rectangle.painted_bytes());
    for row in (height - usize::from(rectangle.paint_height)..height).rev() {
        for column in 0..usize::from(rectangle.paint_width) {
            let at = (row * width + column) * 3;
            expected.extend_from_slice(&[reference[at], reference[at + 1], reference[at + 2], 0]);
        }
    }
    assert_eq!(ours, expected, "a {width}x{height} planar rectangle decoded two ways");
}

/// Paint one decoded rectangle onto the desktop, and remember that it was painted.
fn blit(
    desktop: &mut [u8],
    painted: &mut [bool],
    width: usize,
    rectangle: &bitmap::Bitmap<'_>,
    pixels: &[u8],
) {
    let (x, y) = (usize::from(rectangle.x), usize::from(rectangle.y));
    let columns = usize::from(rectangle.paint_width);
    for row in 0..usize::from(rectangle.paint_height) {
        let from = row * columns * 4;
        let to = ((y + row) * width + x) * 4;
        assert!(to + columns * 4 <= desktop.len(), "a rectangle outside a {width}-wide desktop");
        desktop[to..to + columns * 4].copy_from_slice(&pixels[from..from + columns * 4]);
        painted[(y + row) * width + x..(y + row) * width + x + columns].fill(true);
    }
}

/// The desktop as a `P6` portable pixmap, which needs no encoder and which every
/// image viewer on this machine opens.
fn write_picture(desktop: &[u8], width: usize, height: usize) {
    let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
    let (pixels, _) = desktop.as_chunks::<4>();
    ppm.extend(pixels.iter().flat_map(|pixel| [pixel[0], pixel[1], pixel[2]]));
    std::fs::write(PICTURE, ppm).expect("write the decoded desktop");
    println!("   the decoded desktop is in {PICTURE}");
}

fn count(seen: &mut Vec<(&str, usize)>, what: &'static str) {
    match seen.iter_mut().find(|(name, _)| *name == what) {
        Some((_, count)) => *count += 1,
        None => seen.push((what, 1)),
    }
}

/// What an update type is called, for a line an operator reads.
fn name(code: u8) -> &'static str {
    match code {
        fastpath::ORDERS => "orders",
        fastpath::BITMAP => "bitmap",
        fastpath::PALETTE => "palette",
        fastpath::SYNCHRONIZE => "synchronize",
        fastpath::SURFACE_COMMANDS => "surface commands",
        fastpath::POINTER_HIDDEN => "pointer hidden",
        fastpath::POINTER_DEFAULT => "pointer default",
        fastpath::POINTER_POSITION => "pointer position",
        fastpath::COLOR_POINTER => "colour pointer",
        fastpath::CACHED_POINTER => "cached pointer",
        fastpath::NEW_POINTER => "new pointer",
        fastpath::LARGE_POINTER => "large pointer",
        _ => "something unnamed",
    }
}

/// One whole frame of either framing, told apart by its first byte.
async fn read_any(stream: &mut (impl AsyncRead + AsyncWrite + Unpin)) -> Vec<u8> {
    let mut first = [0_u8; 1];
    stream.read_exact(&mut first).await.expect("read the first byte of a frame");
    if !fastpath::is_output(first[0]) {
        let mut header = [0_u8; TPKT_HEADER];
        header[0] = first[0];
        stream.read_exact(&mut header[1..]).await.expect("read a TPKT header");
        let length = frame_length(&header).expect("a TPKT frame");
        let mut frame = vec![0_u8; length];
        frame[..TPKT_HEADER].copy_from_slice(&header);
        stream.read_exact(&mut frame[TPKT_HEADER..]).await.expect("read the rest of a frame");
        return frame;
    }

    // A fast-path header is two bytes, or three when the second says so.
    let mut frame = first.to_vec();
    let length = loop {
        match fastpath::frame_length(&frame).expect("a fast-path output header") {
            Some(length) => break length,
            None => {
                let mut byte = [0_u8; 1];
                stream.read_exact(&mut byte).await.expect("read a fast-path length");
                frame.push(byte[0]);
            }
        }
    };
    let at = frame.len();
    frame.resize(length, 0);
    stream.read_exact(&mut frame[at..]).await.expect("read the rest of a fast-path frame");
    frame
}

/// Every input event this client can send, encoded by both stacks and compared byte
/// for byte.
///
/// Input is the half of the protocol a probe cannot check by watching: the host does
/// not echo a keystroke back, and a mouse event with the wrong bit set moves the
/// pointer somewhere plausible rather than failing. What can be checked is that the
/// bytes are the ones the stack being replaced would have sent — that stack having
/// driven this host for months — so that is what this does. It needs no server.
#[test]
fn our_input_events_encode_to_the_bytes_ironrdp_sends() {
    let ours = [
        Event::Key { scancode: 0x1E, extended: false, down: true },
        Event::Key { scancode: 0x1E, extended: false, down: false },
        Event::Key { scancode: 0x48, extended: true, down: true },
        Event::Key { scancode: 0x5B, extended: true, down: false },
        Event::Move { x: 0, y: 0 },
        Event::Move { x: 1919, y: 1079 },
        Event::Button { button: Button::Left, down: true, x: 10, y: 20 },
        Event::Button { button: Button::Left, down: false, x: 10, y: 20 },
        Event::Button { button: Button::Middle, down: true, x: 30, y: 40 },
        Event::Button { button: Button::Right, down: true, x: 50, y: 60 },
        Event::Button { button: Button::Right, down: false, x: 50, y: 60 },
        Event::Button { button: Button::X1, down: true, x: 70, y: 80 },
        Event::Button { button: Button::X2, down: false, x: 70, y: 80 },
        Event::Wheel { rotation: 120, horizontal: false, x: 1, y: 2 },
        Event::Wheel { rotation: -120, horizontal: false, x: 1, y: 2 },
        Event::Wheel { rotation: 255, horizontal: true, x: 3, y: 4 },
        Event::Wheel { rotation: -255, horizontal: true, x: 3, y: 4 },
        Event::Wheel { rotation: 0, horizontal: false, x: 5, y: 6 },
    ];

    // One event at a time, so a disagreement names the event that caused it.
    for event in ours {
        let mine = input::pdus(&[event]).next().expect("one event is one PDU");
        let theirs = encode_vec(&FastPathInput::single(translate(event))).expect("IronRDP encodes it");
        assert_eq!(hex(&mine), hex(&theirs), "{event:?}");
    }

    // Then all of them twice over in one PDU, which crosses both the fifteen-event
    // count and the 127-byte length — the two places the header changes shape.
    let batch: Vec<_> = ours.iter().chain(ours.iter()).copied().collect();
    let mine = input::pdus(&batch).next().expect("one PDU holds them all");
    assert!(mine.len() > 0x7F, "the batch is long enough to need a two-byte length");
    let events: Vec<_> = batch.iter().copied().map(translate).collect();
    let theirs = encode_vec(&FastPathInput::new(events).expect("a batch")).expect("IronRDP encodes it");
    assert_eq!(hex(&mine), hex(&theirs), "a batch of {} events", batch.len());
}

/// The same event, said IronRDP's way.
fn translate(event: Event) -> FastPathInputEvent {
    let mouse = |flags, rotation, x, y| {
        FastPathInputEvent::MouseEvent(MousePdu {
            flags,
            number_of_wheel_rotation_units: rotation,
            x_position: x,
            y_position: y,
        })
    };
    match event {
        Event::Key { scancode, extended, down } => {
            let mut flags = KeyboardFlags::empty();
            if extended {
                flags |= KeyboardFlags::EXTENDED;
            }
            if !down {
                flags |= KeyboardFlags::RELEASE;
            }
            FastPathInputEvent::KeyboardEvent(flags, scancode)
        }
        Event::Move { x, y } => mouse(PointerFlags::MOVE, 0, x, y),
        Event::Button { button: button @ (Button::X1 | Button::X2), down, x, y } => {
            let mut flags = if button == Button::X1 {
                PointerXFlags::BUTTON1
            } else {
                PointerXFlags::BUTTON2
            };
            if down {
                flags |= PointerXFlags::DOWN;
            }
            FastPathInputEvent::MouseEventEx(MouseXPdu { flags, x_position: x, y_position: y })
        }
        Event::Button { button, down, x, y } => {
            let mut flags = match button {
                Button::Left => PointerFlags::LEFT_BUTTON,
                Button::Middle => PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
                _ => PointerFlags::RIGHT_BUTTON,
            };
            if down {
                flags |= PointerFlags::DOWN;
            }
            mouse(flags, 0, x, y)
        }
        Event::Wheel { rotation, horizontal, x, y } => {
            let flags = if horizontal {
                PointerFlags::HORIZONTAL_WHEEL
            } else {
                PointerFlags::VERTICAL_WHEEL
            };
            mouse(flags, rotation, x, y)
        }
    }
}
