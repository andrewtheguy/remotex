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
//! It goes as far as the new stack can carry a connection and stops there — at the
//! moment the desktop is live and the first update would arrive. What it proves at
//! each step is the step a server is the only judge of: that the host accepts what we
//! sent, and that what it sends back decodes.

mod common;

use std::time::Duration;

use remotex::rdp_client::proto::capabilities::{ConfirmActive, DemandActive};
use remotex::rdp_client::proto::credssp::{self, Credentials};
use remotex::rdp_client::proto::finalization::{self, Response};
use remotex::rdp_client::proto::gcc::{Channel, ConferenceCreateRequest, ConferenceCreateResponse};
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
