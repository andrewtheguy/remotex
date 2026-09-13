//! `--listen unix:<path>`: the gateway on a socket instead of a port.
//!
//! It exists for a gateway that only ever answers a reverse proxy on the same
//! machine, so what is worth proving here is the whole path a proxy takes — a real
//! process, started the way a service manager starts it, answering an HTTP request
//! written on the socket by hand. The unit tests in `src/main.rs` cover what
//! `bind_unix` decides; this one covers that the decision reaches an actual server.
//!
//! No browser is involved and none could be: a page addresses its gateway by URL.

#![cfg(unix)]

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod common;

/// How long to wait for a freshly spawned gateway to be listening. Generous: it is
/// a bound on a machine being slow, not a measurement of anything.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// A gateway config with a login and one target it never dials.
fn config_text() -> String {
    let site_passwd = remotex::auth::generate("admin", "hunter2", 4).unwrap();
    format!(
        r#"
[server]
site_passwd = "{site_passwd}"

[[targets]]
name = "unreachable"
protocol = "rdp"
host = "192.0.2.10"
"#
    )
}

/// The child, killed when the test ends however it ends.
struct Gateway(Child);

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(config: &Path, socket: &Path) -> Gateway {
    Gateway(
        Command::new(env!("CARGO_BIN_EXE_remotex"))
            .arg("serve")
            .arg("-c")
            .arg(config)
            .arg("--listen")
            .arg(format!("unix:{}", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the gateway binary starts"),
    )
}

/// Wait for a freshly spawned gateway to be listening, or fail saying it never was.
///
/// The one place that retries, and only for a gateway that has not started *yet*.
/// A request that retried would wait out the whole timeout for a gateway that has
/// died, and then report it as one that never came up.
fn wait_until_ready(socket: &Path) {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        match UnixStream::connect(socket) {
            Ok(_) => return,
            Err(e) if Instant::now() >= deadline => {
                panic!("nothing was listening on {} after {READY_TIMEOUT:?}: {e}", socket.display())
            }
            Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

/// One HTTP/1.1 request written straight onto the socket — which is exactly what a
/// reverse proxy does, and the reason nothing here builds a URL.
fn request(socket: &Path, path: &str) -> String {
    let mut stream = UnixStream::connect(socket)
        .unwrap_or_else(|e| panic!("cannot reach {}: {e}", socket.display()));
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: gateway\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .expect("the request is written");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("the gateway answers");
    response
}

/// The whole path: a gateway started on a socket answers a request made on it, the
/// socket is reachable only by its owner and group, and stopping the gateway takes
/// the file away again.
#[test]
fn a_gateway_on_a_unix_socket_answers_and_cleans_up_after_itself() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = common::ScratchDir::new("unix-listen");
    let config = dir.write("remotex.toml", &config_text());
    let socket = dir.path().join("gateway.sock");

    let mut gateway = start(&config, &socket);
    wait_until_ready(&socket);
    let response = request(&socket, "/api/health");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.ends_with("ok"), "the health handler's own answer: {response}");

    // The point of being on a socket at all: the filesystem decides who may connect.
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o660, "owner and group, and nobody else");

    // A login is still a login — the proxy in front of this is a transport, not a
    // credential, and nothing about the socket lets a request past the door.
    let refused = request(&socket, "/api/targets");
    assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");

    // SIGTERM rather than `kill`, because what is under test is the shutdown path:
    // an installed handler stops the servers and the guard removes the file.
    let pid = gateway.0.id();
    assert!(
        Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .expect("kill runs")
            .success(),
        "the gateway is signalled"
    );

    // Waited for here rather than in `Gateway::drop`, which kills what it finds: a
    // `SIGKILL` racing the handler would take the process away mid-shutdown, and the
    // socket would then still be there for a reason that is not the one under test.
    // Once this returns, the guard has run or it never will.
    let status = gateway.0.wait().expect("the gateway exits");
    assert!(status.success(), "a signalled gateway stops cleanly: {status}");
    assert!(
        !socket.exists(),
        "a stopped gateway must leave no socket behind: {} is still there",
        socket.display()
    );
}

/// A gateway that was killed leaves its socket file behind, and the next start has
/// to take it over — otherwise a service that was `SIGKILL`ed cannot come back
/// without somebody deleting a file by hand.
#[test]
fn a_leftover_socket_does_not_stop_the_next_start() {
    let dir = common::ScratchDir::new("unix-leftover");
    let config = dir.write("remotex.toml", &config_text());
    let socket = dir.path().join("gateway.sock");

    // What a killed process leaves: the file, with nothing serving it.
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());

    let _gateway = start(&config, &socket);
    wait_until_ready(&socket);
    let response = request(&socket, "/api/health");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
}
