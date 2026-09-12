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
//! It goes as far as the connection can be carried, and then watches the desktop
//! paint itself. What it proves at each step is the step a server is the only judge
//! of: that the host accepts what we sent, and that what it sends back decodes.
//!
//! Beside it are three tests that need no server at all. Every PDU this client sends
//! whose shape a host never acknowledges — a keystroke, a mouse event, a dynamic
//! channel PDU, a monitor layout — is compared against bytes recorded from IronRDP,
//! the stack this one replaced and which drove this host for months. A wrong bit in
//! any of them does not fail: it moves the pointer somewhere plausible, or has the
//! host quietly never open a channel.
//!
//! What it decodes is also written out, as `tmp/rdp_proto_probe.ppm`, for an operator
//! who would rather look at the desktop than read a coverage figure.

mod common;

use std::time::Duration;

use remotex::rdp_client::proto::bitmap::{self, Scratch};
use remotex::rdp_client::proto::capabilities::{ConfirmActive, DemandActive};
use remotex::rdp_client::proto::credssp::{self, Credentials};
use remotex::rdp_client::proto::fastpath::{self, Fragments};
use remotex::rdp_client::proto::finalization::{self, Response};
use remotex::rdp_client::proto::gcc::{Channel, ConferenceCreateRequest, ConferenceCreateResponse};
use remotex::rdp_client::proto::channel::Chunk;
use remotex::rdp_client::proto::{channel, cliprdr, display, dvc};
use remotex::rdp_client::proto::input::{self, Button, Event};
use remotex::rdp_client::proto::pointer::{self, Pointer};
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
const CHANNELS: [Channel; 2] = [Channel::DYNAMIC, Channel::CLIPBOARD];

/// A size to open at. Nothing in this probe depends on it; the server just has to
/// accept it.
const DESKTOP: (u16, u16) = (1920, 1080);

/// What to resize it to, which nothing but a monitor layout can bring about.
const RESIZED: (u16, u16) = (1280, 800);

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
        // The virtual channels that were asked for, numbered by the server in the
        // order they were named: the transport Display Control is opened over, and
        // the clipboard's own.
        let (dynamic, clipboard) = (channels[0], channels[1]);
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
        //     paints itself, and what arrives is whatever the host decided to send —
        //     including the dynamic channel it opens, which is answered as it comes.
        let watched =
            watch(&mut stream, &demand, user, Channels { io_channel, dynamic, clipboard }).await;
        let display = watched.display;

        // 12. The resize. A monitor layout is the one thing this client says on a
        //     virtual channel, and the host's answer is to tear the share down and
        //     build it again at the size that was asked for — which is the only way
        //     to see from out here that every layer under it was right.
        // The clipboard is the host's answer too: it opens the channel a moment after
        // the share goes live, and the Format List Response is it vouching for
        // everything underneath — the channel's options and chunk flags, the
        // capabilities, and a list written in the short-name form the capability
        // exchange settled on. A host that disliked any of those would answer nothing
        // at all and never say why, so this assertion is the whole of what the far end
        // can be asked about this channel.
        assert!(watched.clipboard_ready, "the host opened its clipboard channel");
        assert_eq!(
            watched.clipboard_list_taken,
            Some(true),
            "the host took the format list this client advertised"
        );

        let control = display.channel.expect("the host opened Display Control");
        assert!(
            display.caps.is_some(),
            "Display Control is not usable until its capabilities arrive"
        );
        let layout = display::monitor_layout(RESIZED.0.into(), RESIZED.1.into(), 100);
        let pdu = dvc::data(control, &layout).unwrap();
        for chunk in channel::chunks(&pdu, demand.chunk, Channel::DYNAMIC.chunk_flags()).unwrap()
        {
            send(&mut stream, user, dynamic, &chunk).await;
        }
        println!("-> a monitor layout of {}x{}", RESIZED.0, RESIZED.1);

        let demand = reactivation(&mut stream, io_channel).await;
        println!(
            "<- Demand Active: share {:#x}, desktop {}x{}",
            demand.share_id, demand.width, demand.height
        );
        assert_eq!(
            (demand.width, demand.height),
            RESIZED,
            "the host rebuilt the desktop at the size the monitor layout asked for"
        );
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
    let mcs::Indication::Data(data) = mcs::send_data_indication(&frame).expect("a Send Data")
    else {
        panic!("the host left the conference");
    };
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
    user: u16,
    channels: Channels,
) -> Watched {
    let Channels { io_channel, dynamic, clipboard } = channels;
    let (width, height) = (usize::from(demand.width), usize::from(demand.height));
    let mut desktop = vec![0_u8; width * height * 4];
    let mut painted = vec![false; width * height];

    let mut fragments = Fragments::new(demand.multifragment);
    let mut scratch = Scratch::default();
    let mut pixels = Vec::new();
    let mut cursors = pointer::Cache::new();
    let mut chunks = channel::Reassembly::new();
    let mut incoming = dvc::Incoming::new();
    let mut clip_chunks = channel::Reassembly::new();
    let mut watched = Watched::default();

    let mut seen: Vec<(&str, usize)> = Vec::new();
    let mut rectangles = 0_usize;
    let mut compressed = 0_usize;
    let mut bytes = 0_usize;

    let deadline = tokio::time::Instant::now() + WATCH;
    while let Ok(frame) = tokio::time::timeout_at(deadline, read_any(stream)).await {
        if !fastpath::is_output(frame[0]) {
            // The slow path still carries everything that is not an update. During a
            // quiet session that is a Set Error Info saying nothing is wrong.
            let mcs::Indication::Data(data) =
                mcs::send_data_indication(&frame).expect("a Send Data")
            else {
                panic!("the host left the conference");
            };
            if data.channel == dynamic {
                // The dynamic virtual channel, which the server opens as soon as the
                // share is live. Everything it says is answered here, because a
                // channel whose Create Request goes unanswered is never opened — and
                // Display Control is the one this client is after.
                let reply = {
                    let pdu = match chunks.push(data.payload).expect("a channel PDU") {
                        Chunk::Whole(pdu) => pdu,
                        other => {
                            assert_eq!(other, Chunk::Partial, "a dynamic channel PDU was dropped");
                            continue;
                        }
                    };
                    let Some(message) = incoming.push(pdu).expect("a dynamic channel PDU") else {
                        continue;
                    };
                    answer(message, &mut watched.display)
                };
                if let Some(reply) = reply {
                    let flags = Channel::DYNAMIC.chunk_flags();
                    for chunk in
                        channel::chunks(&reply, demand.chunk, flags).expect("a short reply")
                    {
                        send(stream, user, dynamic, &chunk).await;
                    }
                }
                continue;
            }
            if data.channel == clipboard {
                // The clipboard channel, which the host also opens on its own once
                // the share is live. Its opening PDU is the only one that needs an
                // answer, and the answer is what settles the form of every format
                // list either end sends afterwards.
                let replies = {
                    let pdu = match clip_chunks.push(data.payload).expect("a channel PDU") {
                        Chunk::Whole(pdu) => pdu,
                        other => {
                            assert_eq!(other, Chunk::Partial, "a clipboard PDU was dropped");
                            continue;
                        }
                    };
                    match cliprdr::decode(pdu).expect("a clipboard PDU") {
                        cliprdr::Message::Capabilities { version, flags } => {
                            println!("<- cliprdr: version {version}, flags {flags:#x}");
                            Vec::new()
                        }
                        cliprdr::Message::MonitorReady => {
                            println!("<- cliprdr: monitor ready");
                            watched.clipboard_ready = true;
                            // The capabilities, then an empty format list: this end
                            // has copied nothing, and saying so is what tells the
                            // host there is a clipboard here at all.
                            vec![cliprdr::capabilities(), cliprdr::format_list(&[])]
                        }
                        cliprdr::Message::ListResponse { ok } => {
                            println!("<- cliprdr: format list response, ok {ok}");
                            watched.clipboard_list_taken = Some(ok);
                            Vec::new()
                        }
                        cliprdr::Message::Formats(list) => {
                            let formats = cliprdr::formats(list).expect("a format list");
                            println!("<- cliprdr: the remote holds {formats:?}");
                            watched.clipboard_formats = Some(formats);
                            vec![cliprdr::format_list_response()]
                        }
                        other => {
                            println!("<- cliprdr: {other:?}");
                            Vec::new()
                        }
                    }
                };
                for reply in replies {
                    // The clipboard's chunks wear `CHANNEL_FLAG_SHOW_PROTOCOL` and the
                    // dynamic channel's must not. A host that disagrees with either
                    // stops answering that channel and says nothing about why, which is
                    // most of what this branch is here to catch.
                    let flags = Channel::CLIPBOARD.chunk_flags();
                    for chunk in
                        channel::chunks(&reply, demand.chunk, flags).expect("a short reply")
                    {
                        send(stream, user, clipboard, &chunk).await;
                    }
                }
                continue;
            }
            assert_eq!(data.channel, io_channel, "a PDU on a channel nothing asked for");
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
                // Every pointer update is decoded. A cursor is small enough to print
                // one line each: there are a dozen in a quiet five seconds, and their
                // depth is what decides this module's scope.
                let decoded = cursors.update(update.code, update.data).expect("a pointer update");
                match &decoded {
                    Pointer::Shape(shape) => {
                        let depth = depth(update.code, update.data);
                        println!("<- {}: xorBpp {depth}, {shape:?}", name(update.code));
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
    watched
}

/// The virtual channels one session's PDUs are addressed to.
#[derive(Clone, Copy)]
struct Channels {
    io_channel: u16,
    dynamic: u16,
    clipboard: u16,
}

/// What the host said on its two virtual channels while the desktop painted.
#[derive(Debug, Default)]
struct Watched {
    display: Display,
    /// Whether the host opened its clipboard and sent the Monitor Ready that starts
    /// the negotiation.
    clipboard_ready: bool,
    /// What it made of the format list this end advertised, which is the host
    /// vouching for the short-name form it was written in.
    clipboard_list_taken: Option<bool>,
    /// What the remote clipboard held, if a copy happened to be announced while this
    /// was watching. Nothing provokes one, so it is usually `None`.
    clipboard_formats: Option<Vec<u32>>,
}

/// The Display Control channel, once the server has opened it.
#[derive(Debug, Default)]
struct Display {
    /// The number the server's Create Request gave it.
    channel: Option<u32>,
    /// What it said it would lay out, which arrives after the channel is open and
    /// before a layout may be sent.
    caps: Option<display::Capabilities>,
}

/// What to say back to one dynamic channel PDU.
///
/// The one channel this client takes is Display Control; every other name a Windows
/// host offers — a printer, a smart card, a camera — is refused by name, which is
/// what a client with nothing behind them does.
fn answer(message: dvc::Message<'_>, display: &mut Display) -> Option<Vec<u8>> {
    match message {
        dvc::Message::Capabilities { version } => {
            println!("<- drdynvc: capabilities version {version}");
            Some(dvc::capabilities_response(version))
        }
        dvc::Message::Create { channel, name } => {
            let wanted = name == display::CHANNEL_NAME;
            let status = if wanted { dvc::ACCEPTED } else { dvc::NO_LISTENER };
            println!(
                "<- drdynvc: create {channel} for {name} ({})",
                if wanted { "taken" } else { "refused" }
            );
            if wanted {
                display.channel = Some(channel);
            }
            Some(dvc::create_response(channel, status))
        }
        dvc::Message::Close { channel } => {
            println!("<- drdynvc: close {channel}");
            if display.channel == Some(channel) {
                *display = Display::default();
            }
            None
        }
        dvc::Message::Data { channel, data } => {
            if display.channel != Some(channel) {
                println!("<- drdynvc: {} bytes on channel {channel}", data.len());
                return None;
            }
            let caps = display::capabilities(data).expect("the Display Control capabilities");
            println!("<- display control: {caps:?}");
            display.caps = Some(caps);
            None
        }
    }
}

/// Read until the server has torn the share down and demanded a new one.
///
/// Everything in between is the session going about its business — fast-path updates
/// for a desktop that is about to be replaced, and whatever the slow path carries —
/// and none of it is what this is waiting for.
async fn reactivation(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    io_channel: u16,
) -> DemandActive {
    let mut deactivated = false;
    loop {
        let frame = tokio::time::timeout(WATCH, read_any(stream))
            .await
            .expect("the host answers a monitor layout within the watch window");
        if fastpath::is_output(frame[0]) {
            continue;
        }
        let mcs::Indication::Data(data) = mcs::send_data_indication(&frame).expect("a Send Data")
        else {
            panic!("the host left the conference");
        };
        if data.channel != io_channel {
            continue;
        }
        match share::decode(data.payload).expect("a share control PDU") {
            Pdu::DeactivateAll => {
                println!("<- Deactivate All: the host is rebuilding the desktop");
                deactivated = true;
            }
            Pdu::DemandActive(body) => {
                assert!(deactivated, "a Demand Active without the Deactivate All before it");
                return DemandActive::decode(body).expect("an RDP Demand Active PDU");
            }
            Pdu::Data(_) => {}
        }
    }
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

/// Every input event this client can send, against the bytes IronRDP sent for it.
///
/// Input is the half of the protocol a probe cannot check by watching: the host does
/// not echo a keystroke back, and a mouse event with the wrong bit set moves the
/// pointer somewhere plausible rather than failing. What can be checked is that the
/// bytes are the ones the stack this one replaced would have sent — that stack having
/// driven this host for months — so these were recorded from it and frozen. They need
/// no server.
#[test]
fn our_input_events_encode_to_the_bytes_ironrdp_sends() {
    /// One event, and the whole fast-path PDU IronRDP wrapped it in.
    const SINGLES: [(Event, &str); 18] = [
        (Event::Key { scancode: 0x1E, extended: false, down: true }, "0404001e"),
        (Event::Key { scancode: 0x1E, extended: false, down: false }, "0404011e"),
        (Event::Key { scancode: 0x48, extended: true, down: true }, "04040248"),
        (Event::Key { scancode: 0x5B, extended: true, down: false }, "0404035b"),
        (Event::Move { x: 0, y: 0 }, "040920000800000000"),
        (Event::Move { x: 1919, y: 1079 }, "04092000087f073704"),
        (Event::Button { button: Button::Left, down: true, x: 10, y: 20 }, "04092000900a001400"),
        (Event::Button { button: Button::Left, down: false, x: 10, y: 20 }, "04092000100a001400"),
        (Event::Button { button: Button::Middle, down: true, x: 30, y: 40 }, "04092000c01e002800"),
        (Event::Button { button: Button::Right, down: true, x: 50, y: 60 }, "04092000a032003c00"),
        (Event::Button { button: Button::Right, down: false, x: 50, y: 60 }, "040920002032003c00"),
        (Event::Button { button: Button::X1, down: true, x: 70, y: 80 }, "040940018046005000"),
        (Event::Button { button: Button::X2, down: false, x: 70, y: 80 }, "040940020046005000"),
        (Event::Wheel { rotation: 120, horizontal: false, x: 1, y: 2 }, "040920780201000200"),
        (Event::Wheel { rotation: -120, horizontal: false, x: 1, y: 2 }, "040920880301000200"),
        (Event::Wheel { rotation: 255, horizontal: true, x: 3, y: 4 }, "040920ff0403000400"),
        (Event::Wheel { rotation: -255, horizontal: true, x: 3, y: 4 }, "040920010503000400"),
        (Event::Wheel { rotation: 0, horizontal: false, x: 5, y: 6 }, "040920000205000600"),
    ];

    // One event at a time, so a disagreement names the event that caused it.
    for (event, theirs) in SINGLES {
        let mine = input::pdus(&[event]).next().expect("one event is one PDU");
        assert_eq!(hex(&mine).replace(' ', ""), theirs, "{event:?}");
    }

    // Then all of them twice over in one PDU, which crosses both the fifteen-event
    // count and the 127-byte length — the two places the header changes shape.
    let ours: Vec<Event> = SINGLES.iter().map(|(event, _)| *event).collect();
    let batch: Vec<Event> = ours.iter().chain(ours.iter()).copied().collect();
    let mine = input::pdus(&batch).next().expect("one PDU holds them all");
    assert!(mine.len() > 0x7F, "the batch is long enough to need a two-byte length");
    assert_eq!(hex(&mine).replace(' ', ""), BATCH, "a batch of {} events", batch.len());
}

/// The 36 events of [`our_input_events_encode_to_the_bytes_ironrdp_sends`] in one
/// PDU, as IronRDP wrote it.
const BATCH: &str = concat!(
    "0080d824001e011e0248035b200008000000002000087f0737042000900a0014002000100a0014002000c01e0028",
    "002000a032003c0020002032003c004001804600500040020046005000207802010002002088030100020020ff04",
    "030004002001050300040020000205000600001e011e0248035b200008000000002000087f0737042000900a0014",
    "002000100a0014002000c01e0028002000a032003c0020002032003c004001804600500040020046005000207802",
    "010002002088030100020020ff04030004002001050300040020000205000600",
);

/// Every PDU this client sends on a virtual channel, against the bytes IronRDP sent.
///
/// The dynamic channel's first byte packs a command and the width of the two fields
/// after it, so a channel number that crosses a width boundary changes the shape of
/// the PDU rather than one field in it — which is exactly the kind of mistake a live
/// host answers by quietly never opening the channel. Every boundary is here.
#[test]
fn our_dynamic_channel_pdus_encode_to_the_bytes_ironrdp_sends() {
    for (version, theirs) in [(1, "50000100"), (2, "50000200"), (3, "50000300")] {
        assert_eq!(hex(&dvc::capabilities_response(version)).replace(' ', ""), theirs);
    }

    /// A channel number, then the accepted create response, the refusal, the close
    /// and a three-byte data PDU for it.
    const CHANNELS: [(u32, &str, &str, &str, &str); 6] = [
        (0x3, "100300000000", "1003010000c0", "4003", "3003010203"),
        (0xFF, "10ff00000000", "10ff010000c0", "40ff", "30ff010203"),
        (0x100, "11000100000000", "110001010000c0", "410001", "310001010203"),
        (0xFFFF, "11ffff00000000", "11ffff010000c0", "41ffff", "31ffff010203"),
        (0x0001_0000, "120000010000000000", "1200000100010000c0", "4200000100", "3200000100010203"),
        (u32::MAX, "12ffffffff00000000", "12ffffffff010000c0", "42ffffffff", "32ffffffff010203"),
    ];
    for (channel, accepted, refused, closed, data) in CHANNELS {
        let ours = dvc::create_response(channel, dvc::ACCEPTED);
        assert_eq!(hex(&ours).replace(' ', ""), accepted, "a create response for {channel:#x}");
        let ours = dvc::create_response(channel, dvc::NO_LISTENER);
        assert_eq!(hex(&ours).replace(' ', ""), refused, "a refusal for {channel:#x}");
        assert_eq!(hex(&dvc::close(channel)).replace(' ', ""), closed, "a close for {channel:#x}");
        let ours = dvc::data(channel, &[1, 2, 3]).unwrap();
        assert_eq!(hex(&ours).replace(' ', ""), data, "a data PDU for {channel:#x}");
    }
}

/// The monitor layout, against the one IronRDP built from the same request.
///
/// This is the whole of what this client says on a dynamic channel, and the fields it
/// leaves at zero — orientation, physical size — are the ones the stack it replaced
/// was already being told to leave at zero.
#[test]
fn our_monitor_layout_encodes_to_the_bytes_ironrdp_sends() {
    const LAYOUTS: [(u32, u32, u32, &str); 3] = [
        (1280, 800, 100, concat!(
            "02000000380000002800000001000000010000000000000000000000",
            "00050000200300000000000000000000000000006400000064000000"
        )),
        (1920, 1080, 150, concat!(
            "02000000380000002800000001000000010000000000000000000000",
            "80070000380400000000000000000000000000009600000064000000"
        )),
        (3840, 2160, 500, concat!(
            "02000000380000002800000001000000010000000000000000000000",
            "000f000070080000000000000000000000000000f401000064000000"
        )),
    ];
    for (width, height, scale, theirs) in LAYOUTS {
        let ours = display::monitor_layout(width, height, scale);
        assert_eq!(
            hex(&ours).replace(' ', ""),
            theirs,
            "a {width}x{height} layout at {scale}%"
        );
    }
}
