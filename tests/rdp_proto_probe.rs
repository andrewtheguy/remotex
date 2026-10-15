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
//!
//! ## What a host draws with, without the graphics pipeline
//!
//! Lines starting `PROBE` measure the bitmap path, best with something moving on the
//! host's desktop:
//!
//! - Always: the bytes per second, and the Bitmap Updates grouped into bursts, with
//!   the rows each burst covered. Measured on 2026-10-14 against a Windows 11 host
//!   on Arm with a 16 fps animation: a burst of about 30 planar tiles every 15 ms,
//!   sweeping the animated area top to bottom in about 180 ms, whatever this end
//!   advertised or how fast it read.
//! - [`SURFACE_ENV`] (`frames`, `nsc`, `rfx`, or a list) also offers Surface
//!   Commands with frame markers, Frame Acknowledge, and the codecs named, and
//!   reports what the host drew with: the codec, each frame's tiles and
//!   begin-to-end time, and NSCodec's color loss and subsampling levels.
//!   [`NSC_ENV`] is the NSCodec capability offered. That host drew with NSCodec only
//!   when dynamic fidelity was allowed, and then at color loss level 3 with chroma
//!   subsampled whatever was offered, each frame whole within 45 ms; it offers no
//!   RemoteFX.
//! - `REMOTEX_PROBE_ACK=0` withholds the frame acknowledgements,
//!   `REMOTEX_PROBE_UNACKED` is the unacknowledged-frame count advertised,
//!   `REMOTEX_PROBE_MULTIFRAGMENT` the multifragment size, `REMOTEX_PROBE_NODECODE`
//!   reads the updates without decoding them, and [`WATCH_ENV`] is how many seconds
//!   to watch.

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
            monitors: 1,
            placement: remotex::config::Placement::Right,
            scale_percent: 0,
            client_name: "remotex",
            keyboard_layout: KEYBOARD_LAYOUT,
            selected_protocol: protocol.bits(),
            channels: &CHANNELS,
            graphics: false,
            autodetect: false,
            multitransport: 0,
        }
        .encode();
        let initial = mcs::connect_initial(&conference).expect("the conference fits a frame");
        println!("-> MCS Connect-Initial, {} bytes", initial.len());
        stream.write_all(&initial).await.expect("send the Connect-Initial");
        stream.flush().await.expect("send the Connect-Initial");

        let frame = read_frame(&mut stream).await;
        println!("<- MCS Connect-Response, {} bytes", frame.len());
        let answer = mcs::connect_response(&frame).expect("an MCS Connect-Response");
        let ConferenceCreateResponse { io_channel, channels, .. } =
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
            audio: false,
            microphone: false,
            keyboard_layout: KEYBOARD_LAYOUT,
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
        let Pdu::DemandActive { source, body } = share::decode(&payload).expect("a share control PDU")
        else {
            panic!("the server did not demand a share once licensing was done");
        };
        let demand = DemandActive::decode(source, body).expect("an RDP Demand Active PDU");
        if std::env::var(SURFACE_ENV).is_ok() {
            server_sets(body);
        }
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
            multifragment: std::env::var("REMOTEX_PROBE_MULTIFRAGMENT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(demand.multifragment),
        }
        .encode();
        let confirm = with_surface_commands(confirm);
        let confirm = share::confirm_active(user, &confirm);
        send(&mut stream, user, io_channel, &confirm).await;
        println!("-> Confirm Active, {} bytes", confirm.len());

        // 10. The finalization handshake. All four go out without waiting; the
        //     server's four come back in its own time, with session PDUs among them.
        for request in finalization::requests(user, demand.server_channel, demand.share_id) {
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
        let layout = display::monitor_layout(&[(RESIZED.0.into(), RESIZED.1.into())], remotex::config::Placement::Right, &[100]);
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

    let mut surface = SurfaceStats::default();
    let mut per_second: std::collections::BTreeMap<u64, usize> = Default::default();
    // When each Bitmap Update arrived, for how a host's frame spreads over time.
    let mut arrivals: Vec<Duration> = Vec::new();
    // Where each rectangle went, and when.
    let mut places: Vec<(Duration, u16, u16)> = Vec::new();
    let started = tokio::time::Instant::now();
    let watch = std::env::var(WATCH_ENV).ok().and_then(|v| v.parse().ok()).map_or(WATCH, Duration::from_secs);
    let deadline = started + watch;
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
                Pdu::DemandActive { body, .. } => format!("a Demand Active of {} bytes", body.len()),
                Pdu::DeactivateAll => "a Deactivate All".to_owned(),
                Pdu::Data(pdu) if pdu.kind == 0x2f && pdu.body.len() >= 4 => {
                    let code = u32::from_le_bytes([pdu.body[0], pdu.body[1], pdu.body[2], pdu.body[3]]);
                    format!("PROBE Set Error Info {code:#010x}")
                }
                Pdu::Data(pdu) => {
                    format!("a data PDU of type {:#04x}, {} bytes", pdu.kind, pdu.body.len())
                }
            };
            println!("<- slow path: {what}");
            continue;
        }
        bytes += frame.len();
        *per_second.entry(started.elapsed().as_secs()).or_insert(0usize) += frame.len();
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
            if update.code == fastpath::SURFACE_COMMANDS {
                surface.read(update.data, started.elapsed().as_secs());
                continue;
            }
            if update.code != fastpath::BITMAP {
                continue;
            }
            arrivals.push(started.elapsed());
            if let Ok(rects) = bitmap::update(update.data) {
                let at = started.elapsed();
                for r in &rects {
                    places.push((at, r.x, r.y));
                }
            }
            if std::env::var("REMOTEX_PROBE_NODECODE").is_ok() {
                rectangles += bitmap::update(update.data).expect("a Bitmap Update").len();
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
        // Every frame the host ended, acknowledged once its updates are in: the
        // host paces its frames by these.
        let acking = std::env::var("REMOTEX_PROBE_ACK").as_deref() != Ok("0");
        for frame_id in surface.unacknowledged.drain(..).filter(|_| acking) {
            let ack = share::data(PDUTYPE2_FRAME_ACKNOWLEDGE, user, demand.share_id, &frame_id.to_le_bytes());
            send(stream, user, io_channel, &ack).await;

        }
    }

    surface.report();
    report_bursts(&arrivals);
    report_band(&places);
    println!("PROBE KB per second: {:?}", per_second.iter().map(|(t, b)| format!("{t}:{}", b / 1024)).collect::<Vec<_>>());
    seen.sort_unstable();
    println!("<- {bytes} bytes of fast-path output: {seen:?}");
    println!("   {rectangles} rectangles, {compressed} of them compressed");
    let covered = painted.iter().filter(|seen| **seen).count();
    println!("   {}% of the desktop painted", covered * 100 / painted.len());
    write_picture(&desktop, width, height);

    // A host that draws with surface commands sends no bitmap updates to count.
    if surface.commands == 0 {
        assert!(rectangles > 0, "a live share paints itself, and nothing arrived");
        assert!(covered > 0, "rectangles arrived and none of them landed on the desktop");
    }
    watched
}

/// Set to a comma-separated list of `nsc` and `rfx` (or `frames` for neither) to
/// advertise Surface Commands with frame markers, Frame Acknowledge and, for the
/// codecs named, Bitmap Codecs ([MS-RDPBCGR] 2.2.7.2.9, 2.2.7.2.10; [MS-RDPRFX]
/// 2.2.1.3), and report what the host draws with in return.
const SURFACE_ENV: &str = "REMOTEX_PROBE_SURFACE";

/// The NSCodec capability advertised, as `fAllowDynamicFidelity,fAllowSubsampling,
/// colorLossLevel` ([MS-RDPNSC] 2.2.1): lossless by default.
const NSC_ENV: &str = "REMOTEX_PROBE_NSC";

/// How many seconds to watch the desktop, in place of [`WATCH`].
const WATCH_ENV: &str = "REMOTEX_PROBE_WATCH";

/// [MS-RDPRFX] 2.2.3.1.
const PDUTYPE2_FRAME_ACKNOWLEDGE: u8 = 0x38;

/// The codec IDs this client assigns: NSCodec's is fixed at 1 ([MS-RDPBCGR]
/// 2.2.7.2.10.1.1), and RemoteFX's is ours to choose.
const NSC_ID: u8 = 1;
const RFX_ID: u8 = 3;

/// `body`, a Confirm Active's, with the capability sets [`SURFACE_ENV`] asks for
/// appended, and its length and count raised to match.
fn with_surface_commands(mut body: Vec<u8>) -> Vec<u8> {
    let Ok(asked) = std::env::var(SURFACE_ENV) else {
        return body;
    };
    let mut sets = Vec::new();
    let mut added = 0u16;
    let mut set = |kind: u16, data: &[u8]| {
        sets.extend_from_slice(&kind.to_le_bytes());
        sets.extend_from_slice(&u16::try_from(data.len() + 4).unwrap().to_le_bytes());
        sets.extend_from_slice(data);
        added += 1;
    };
    // SETSURFACEBITS | FRAMEMARKER | STREAMSURFACEBITS, and a reserved word.
    set(0x001C, &[0x52, 0, 0, 0, 0, 0, 0, 0]);
    // Two frames unacknowledged, as FreeRDP asks, unless told otherwise.
    let unacked: u32 = std::env::var("REMOTEX_PROBE_UNACKED").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
    set(0x001E, &unacked.to_le_bytes());
    let guid = |a: u32, b: u16, c: u16, d: [u8; 8]| {
        let mut g = a.to_le_bytes().to_vec();
        g.extend_from_slice(&b.to_le_bytes());
        g.extend_from_slice(&c.to_le_bytes());
        g.extend_from_slice(&d);
        g
    };
    let mut codecs = Vec::new();
    for codec in asked.split(',').map(str::trim) {
        let (id, properties, g) = match codec {
            "nsc" => {
                let nsc = std::env::var(NSC_ENV).unwrap_or_else(|_| "0,0,1".to_owned());
                let properties: Vec<u8> = nsc.split(',').map(|v| v.trim().parse().unwrap()).collect();
                let g = guid(0xCA8D_1BB9, 0x000F, 0x154F, [0x58, 0x9F, 0xAE, 0x2D, 0x1A, 0x87, 0xE2, 0xD6]);
                (NSC_ID, properties, g)
            }
            "rfx" => {
                // TS_RFX_CLNT_CAPS_CONTAINER of one capset with two icaps, RLGR1
                // and RLGR3, in video mode, mixing with other codecs allowed.
                let mut icaps = Vec::new();
                for entropy in [0x01u8, 0x04] {
                    icaps.extend_from_slice(&0x0100u16.to_le_bytes());
                    icaps.extend_from_slice(&0x0040u16.to_le_bytes());
                    icaps.extend_from_slice(&[0, 1, 1, entropy]);
                }
                let mut capset = 0xCBC1u16.to_le_bytes().to_vec();
                capset.extend_from_slice(&u32::try_from(13 + icaps.len()).unwrap().to_le_bytes());
                capset.push(1);
                capset.extend_from_slice(&0xCFC0u16.to_le_bytes());
                capset.extend_from_slice(&2u16.to_le_bytes());
                capset.extend_from_slice(&8u16.to_le_bytes());
                capset.extend_from_slice(&icaps);
                let mut caps = 0xCBC0u16.to_le_bytes().to_vec();
                caps.extend_from_slice(&8u32.to_le_bytes());
                caps.extend_from_slice(&1u16.to_le_bytes());
                caps.extend_from_slice(&capset);
                let mut container = u32::try_from(12 + caps.len()).unwrap().to_le_bytes().to_vec();
                container.extend_from_slice(&1u32.to_le_bytes());
                container.extend_from_slice(&u32::try_from(caps.len()).unwrap().to_le_bytes());
                container.extend_from_slice(&caps);
                let g = guid(0x7677_2F12, 0xBD72, 0x4463, [0xAF, 0xB3, 0xB7, 0x3C, 0x9C, 0x6F, 0x78, 0x86]);
                (RFX_ID, container, g)
            }
            _ => continue,
        };
        codecs.extend_from_slice(&g);
        codecs.push(id);
        codecs.extend_from_slice(&u16::try_from(properties.len()).unwrap().to_le_bytes());
        codecs.extend_from_slice(&properties);
        println!("-> offering {codec} as codec {id}");
    }
    if !codecs.is_empty() {
        let count = u8::try_from(asked.split(',').filter(|c| matches!(c.trim(), "nsc" | "rfx")).count()).unwrap();
        let mut data = vec![count];
        data.extend_from_slice(&codecs);
        set(0x001D, &data);
    }
    // shareId, originatorId, lengthSourceDescriptor, lengthCombinedCapabilities,
    // the source descriptor, numberCapabilities.
    let source = usize::from(u16::from_le_bytes([body[6], body[7]]));
    let combined = u16::from_le_bytes([body[8], body[9]]) + u16::try_from(sets.len()).unwrap();
    body[8..10].copy_from_slice(&combined.to_le_bytes());
    let at = 10 + source;
    let count = u16::from_le_bytes([body[at], body[at + 1]]) + added;
    body[at..at + 2].copy_from_slice(&count.to_le_bytes());
    body.extend_from_slice(&sets);
    println!("-> surface commands, frame acknowledge{}", if codecs.is_empty() { "" } else { " and bitmap codecs" });
    body
}

/// The capability sets a Demand Active body names, and the bodies of the three
/// [`SURFACE_ENV`] is about: shareId, lengthSourceDescriptor,
/// lengthCombinedCapabilities, the source, numberCapabilities, a pad, the sets.
fn server_sets(body: &[u8]) {
    let source = usize::from(u16::from_le_bytes([body[4], body[5]]));
    let mut at = 8 + source + 4;
    let mut kinds = Vec::new();
    while at + 4 <= body.len() {
        let kind = u16::from_le_bytes([body[at], body[at + 1]]);
        let len = usize::from(u16::from_le_bytes([body[at + 2], body[at + 3]]));
        if len < 4 || at + len > body.len() {
            break;
        }
        kinds.push(format!("{kind:#06x}"));
        if (0x001C..=0x001E).contains(&kind) {
            println!("PROBE server set {kind:#06x}: {:02x?}", &body[at + 4..at + len]);
        }
        at += len;
    }
    println!("PROBE server sets: {}", kinds.join(" "));
}

/// Bitmap Updates grouped into bursts, a burst ending at a gap of more than
/// [`BURST_GAP`]: how long each lasts and how far apart they start, which is what a
/// host with no frame markers spreads one picture over.
const BURST_GAP: Duration = Duration::from_millis(10);

fn report_bursts(arrivals: &[Duration]) {
    if arrivals.len() < 2 {
        return;
    }
    let mut bursts: Vec<(Duration, Duration, usize)> = Vec::new();
    for &at in arrivals {
        match bursts.last_mut() {
            Some((_, end, n)) if at - *end <= BURST_GAP => {
                *end = at;
                *n += 1;
            }
            _ => bursts.push((at, at, 1)),
        }
    }
    // Past the connect's repaints.
    let steady: Vec<_> = bursts.iter().filter(|b| b.0 > Duration::from_secs(2)).collect();
    if steady.len() < 2 {
        return;
    }
    let mut lasting: Vec<u128> = steady.iter().map(|b| (b.1 - b.0).as_millis()).collect();
    let mut apart: Vec<u128> = steady.windows(2).map(|w| (w[1].0 - w[0].0).as_millis()).collect();
    let mut updates: Vec<usize> = steady.iter().map(|b| b.2).collect();
    lasting.sort_unstable();
    apart.sort_unstable();
    updates.sort_unstable();
    let q = |v: &[u128], f: f64| v[((v.len() - 1) as f64 * f) as usize];
    println!(
        "PROBE bursts after 2s: {} ({:.1}/s); lasting ms p10 {} p50 {} p90 {}; starts apart ms p10 {} p50 {} p90 {}; updates each p50 {}",
        steady.len(),
        steady.len() as f64 / (steady.last().unwrap().0 - steady[0].0).as_secs_f64(),
        q(&lasting, 0.1),
        q(&lasting, 0.5),
        q(&lasting, 0.9),
        q(&apart, 0.1),
        q(&apart, 0.5),
        q(&apart, 0.9),
        updates[updates.len() / 2]
    );
    let first: Vec<String> = steady.iter().take(12).map(|b| format!("{}+{}", b.0.as_millis(), (b.1 - b.0).as_millis())).collect();
    println!("PROBE first bursts (start+length ms): {}", first.join(" "));
}

/// The rows each burst's rectangles covered, for the first bursts past two seconds.
fn report_band(places: &[(Duration, u16, u16)]) {
    let mut out = Vec::new();
    let mut burst: Option<(Duration, Duration, u16, u16, u16, u16, usize)> = None;
    for &(at, x, y) in places.iter().filter(|p| p.0 > Duration::from_secs(2)) {
        match &mut burst {
            Some((_, end, top, bottom, left, right, n)) if at - *end <= BURST_GAP => {
                *end = at;
                *top = (*top).min(y);
                *bottom = (*bottom).max(y);
                *left = (*left).min(x);
                *right = (*right).max(x);
                *n += 1;
            }
            _ => {
                if let Some(b) = burst.take() {
                    out.push(b);
                }
                burst = Some((at, at, y, y, x, x, 1));
            }
        }
        if out.len() >= 24 {
            break;
        }
    }
    for (start, _, top, bottom, left, right, n) in out {
        println!("PROBE burst at {}ms: {n} tiles, rows {top}..{bottom}, columns {left}..{right}", start.as_millis());
    }
}

/// One codec's surface bits: commands, bytes, and the bitmap sizes seen.
type CodecTally = (usize, usize, std::collections::BTreeSet<(u16, u16)>);

/// What the host drew with surface commands ([MS-RDPBCGR] 2.2.9.2).
#[derive(Default)]
struct SurfaceStats {
    commands: usize,
    /// By codec ID: commands, bytes, and the sizes seen.
    by_codec: std::collections::BTreeMap<u8, CodecTally>,
    frames_ended: usize,
    /// Frames ended in each second of the watch.
    per_second: std::collections::BTreeMap<u64, usize>,
    unacknowledged: Vec<u32>,
    clock: ClockStart,
    /// NSCodec bitmaps by (color loss level, chroma subsampling level).
    nsc_levels: std::collections::BTreeMap<(u8, u8), usize>,
    began: Option<Duration>,
    in_frame: usize,
    /// Past two seconds: each frame's begin-to-end time, its tiles, and when it ended.
    spans: Vec<(u128, usize, u128)>,
}

/// When the watch began, for the frames' timestamps.
struct ClockStart(std::time::Instant);

impl Default for ClockStart {
    fn default() -> Self {
        Self(std::time::Instant::now())
    }
}

impl ClockStart {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
}

impl SurfaceStats {
    fn read(&mut self, mut data: &[u8], second: u64) {
        let u16_at = |d: &[u8], at: usize| u16::from_le_bytes([d[at], d[at + 1]]);
        while data.len() >= 2 {
            self.commands += 1;
            match u16_at(data, 0) {
                // Set and Stream Surface Bits: a destination, then TS_BITMAP_DATA_EX.
                0x0001 | 0x0006 => {
                    let flags = data[11];
                    let codec = data[13];
                    let (w, h) = (u16_at(data, 14), u16_at(data, 16));
                    let len = u32::from_le_bytes([data[18], data[19], data[20], data[21]]) as usize;
                    let header = 22 + if flags & 0x01 != 0 { 24 } else { 0 };
                    self.in_frame += 1;
                    let entry = self.by_codec.entry(codec).or_default();
                    entry.0 += 1;
                    entry.1 += len;
                    entry.2.insert((w, h));
                    // NSCodec's stream header: four plane byte counts, then the
                    // color loss level and the chroma subsampling level.
                    if codec == NSC_ID && data.len() >= header + 18 {
                        let levels = (data[header + 16], data[header + 17]);
                        *self.nsc_levels.entry(levels).or_default() += 1;
                    }
                    data = &data[(header + len).min(data.len())..];
                }
                0x0004 => {
                    let action = u16_at(data, 2);
                    let id = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);

                    let now = self.clock.elapsed();
                    if action == 0x0000 {
                        self.began = Some(now);
                        self.in_frame = 0;
                    } else if let Some(began) = self.began.take()
                        && now > Duration::from_secs(2)
                    {
                        self.spans.push(((now - began).as_millis(), self.in_frame, now.as_millis()));
                    }
                    if action == 0x0001 {
                        self.frames_ended += 1;
                        *self.per_second.entry(second).or_default() += 1;
                        self.unacknowledged.push(id);
                    }
                    data = &data[8.min(data.len())..];
                }
                other => {
                    println!("PROBE an unknown surface command {other:#06x}; the rest of the update skipped");
                    return;
                }
            }
        }
    }

    fn report(&self) {
        println!("PROBE surface commands: {}, frames ended: {}", self.commands, self.frames_ended);
        for (codec, (n, bytes, sizes)) in &self.by_codec {
            let shown: Vec<_> = sizes.iter().take(6).collect();
            println!("PROBE codec {codec}: {n} commands, {bytes} bytes, sizes {shown:?}{}", if sizes.len() > 6 { " …" } else { "" });
        }
        println!("PROBE frames per second: {:?}", self.per_second.values().collect::<Vec<_>>());
        println!("PROBE NSCodec (color loss level, subsampling): {:?}", self.nsc_levels);
        let shown: Vec<String> = self.spans.iter().take(20).map(|(ms, n, end)| format!("{end}:{n}t/{ms}ms")).collect();
        println!("PROBE frames (ended at: tiles / begin-to-end): {}", shown.join(" "));
    }
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
            Some(dvc::capabilities_response(dvc::answer_version(version)))
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
            Pdu::DemandActive { source, body } => {
                assert!(deactivated, "a Demand Active without the Deactivate All before it");
                return DemandActive::decode(source, body).expect("an RDP Demand Active PDU");
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
        let ours = display::monitor_layout(&[(width, height)], remotex::config::Placement::Right, &[scale]);
        assert_eq!(
            hex(&ours).replace(' ', ""),
            theirs,
            "a {width}x{height} layout at {scale}%"
        );
    }
}
