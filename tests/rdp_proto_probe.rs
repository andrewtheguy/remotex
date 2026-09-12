//! The gateway's own RDP encoding, against servers that did not write it.
//!
//! [`remotex::rdp_client::proto`] is tested against the specification by its own unit
//! tests. This asks the other question — whether a real server agrees — by doing the
//! X.224 exchange over a plain TCP socket with nothing else in the process: our bytes
//! out, the server's bytes in, our decoder on the answer.
//!
//! That is the whole connection sequence this module can carry so far, so the probe
//! stops where the negotiation does, before the TLS handshake a real session would
//! start next. It is deliberately not a session: the point is to find the encoding
//! mistakes that only a server notices, as early as the code that can make them.
//!
//! Two servers, because they disagree in the ways that matter. xrdp is Linux, is
//! usually configured without Network Level Authentication, and comes out of a
//! container:
//!
//! ```sh
//! cargo test --test rdp_proto_probe -- --ignored --nocapture
//! ```
//!
//! A Windows host demands NLA, and comes from the operator's `tmp/test_uat.toml`,
//! named by [`TARGET_ENV`]:
//!
//! ```sh
//! REMOTEX_UAT_TARGET=windows-ent-sandbox \
//!   cargo test --test rdp_proto_probe -- --ignored --nocapture
//! ```

mod common;

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use remotex::rdp_client::proto::x224::{
    ConfirmFlags, ConnectionConfirm, ConnectionRequest, Security, TPKT_HEADER, frame_length,
};

/// Which target in `tmp/test_uat.toml` to dial — see the module docs.
const TARGET_ENV: &str = "REMOTEX_UAT_TARGET";

/// How long a server gets to accept a connection and answer the request.
const BUDGET: Duration = Duration::from_secs(20);

/// What this client asks for: TLS, or TLS with the credentials checked first.
fn offered() -> Security {
    Security::SSL | Security::HYBRID
}

/// Dial, send one Connection Request, and decode what comes back.
fn negotiate(host: &str, port: u16, cookie: &str) -> ConnectionConfirm {
    let address = (host, port)
        .to_socket_addrs()
        .unwrap_or_else(|e| panic!("resolve {host}:{port}: {e}"))
        .next()
        .unwrap_or_else(|| panic!("{host}:{port} resolves to nothing"));
    let mut socket = TcpStream::connect_timeout(&address, BUDGET)
        .unwrap_or_else(|e| panic!("connect to {address}: {e}"));
    socket.set_read_timeout(Some(BUDGET)).expect("set a read timeout");

    let request = ConnectionRequest { cookie: Some(cookie.to_owned()), protocols: offered() };
    let bytes = request.encode();
    println!("-> {} bytes: {}", bytes.len(), hex(&bytes));
    socket.write_all(&bytes).expect("send the connection request");

    // Read the announced length and then exactly that much, which is what the
    // session transport will do for every PDU after this one.
    let mut header = [0_u8; TPKT_HEADER];
    socket.read_exact(&mut header).expect("read a TPKT header");
    let length = frame_length(&header).expect("the answer is a TPKT frame");
    let mut frame = vec![0_u8; length];
    frame[..TPKT_HEADER].copy_from_slice(&header);
    socket.read_exact(&mut frame[TPKT_HEADER..]).expect("read the rest of the frame");
    println!("<- {length} bytes: {}", hex(&frame));

    ConnectionConfirm::decode(&frame).expect("the answer is an X.224 Connection Confirm")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// Whatever the server chose, it has to be something this client offered — that is
/// the one rule the negotiation has, and a client that accepted anything else would
/// go on to speak a protocol it never agreed to.
fn accept(confirm: ConnectionConfirm) {
    match confirm {
        ConnectionConfirm::Negotiated { protocol, flags } => {
            println!(
                "negotiated {protocol:?}, flags {:#04x} (extended client data: {})",
                flags.bits(),
                flags.contains(ConfirmFlags::EXTENDED_CLIENT_DATA)
            );
            assert!(
                offered().contains(protocol),
                "the server chose {protocol:?}, which was not offered"
            );
        }
        ConnectionConfirm::Unnegotiated => {
            panic!("the server answered without negotiation data, meaning legacy RDP security")
        }
        ConnectionConfirm::Refused(why) => panic!("the server refused the negotiation: {why}"),
    }
}

#[test]
#[ignore = "requires Docker or Podman"]
fn xrdp_answers_our_connection_request() {
    common::init_logging();
    let runtime = common::container_runtime();
    let (_container, port) =
        common::start_dummy_server(runtime, "remotex-e2e-xrdp", "xrdp-dummy", 3389);
    let host = common::container_host();
    wait_for_port(&host, port);
    accept(negotiate(&host, port, "probe"));
}

#[test]
#[ignore = "requires a real RDP host from tmp/test_uat.toml"]
fn a_real_host_answers_our_connection_request() {
    common::init_logging();
    let name = std::env::var(TARGET_ENV).unwrap_or_else(|_| {
        panic!("set {TARGET_ENV} to the name of an rdp target in tmp/test_uat.toml")
    });
    let target = common::uat_target(&name);
    println!("rdp_proto_probe: {name} ({}:{})", target.host, target.port);

    // The user name is what a Connection Broker routes on, so send the real one
    // rather than a made-up cookie.
    accept(negotiate(&target.host, target.port, &target.username));
}

/// xrdp is listening some seconds after the container starts, not at the moment it
/// does.
fn wait_for_port(host: &str, port: u16) {
    let deadline = Instant::now() + BUDGET;
    while Instant::now() < deadline {
        if TcpStream::connect((host, port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("nothing accepted a connection on {host}:{port} within {BUDGET:?}");
}
