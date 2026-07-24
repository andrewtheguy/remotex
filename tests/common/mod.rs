//! Shared helpers for the container-backed e2e tests: locate a container
//! runtime, build a dummy-server image, and run it with cleanup-on-drop.
//! (Never a headless browser — see CLAUDE.md.)

use std::path::Path;
use std::process::Command;

/// Locate a container runtime. The dummy remote-desktop server is part of the
/// e2e contract, so a machine without one fails loudly instead of silently
/// skipping the coverage.
pub fn container_runtime() -> &'static str {
    for runtime in ["podman", "docker"] {
        if Command::new(runtime)
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
        {
            return runtime;
        }
    }
    panic!("this e2e test needs podman or docker to start the dummy server");
}

/// Kills the container on drop so a failed test doesn't leak it
/// (`--rm` then removes it).
pub struct Container {
    runtime: &'static str,
    name: String,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(self.runtime)
            .args(["rm", "-f", &self.name])
            .output();
    }
}

/// Build the image from `tests/<context>` (cached after the first run) and
/// start it with the container's `internal_port` published on an ephemeral
/// localhost port. Returns the container guard and the published port.
pub fn start_dummy_server(
    runtime: &'static str,
    image: &str,
    context: &str,
    internal_port: u16,
) -> (Container, u16) {
    let context_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(context);
    let build = Command::new(runtime)
        .args(["build", "-t", image])
        .arg(&context_dir)
        .output()
        .expect("run container build");
    assert!(
        build.status.success(),
        "container build failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    // Grab a free port; the tiny window before the container binds it is fine.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

    let name = format!("{image}-{port}");
    let container = Container { runtime, name: name.clone() };
    let run = Command::new(runtime)
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "-p",
            &format!("127.0.0.1:{port}:{internal_port}"),
            image,
        ])
        .output()
        .expect("run container");
    assert!(
        run.status.success(),
        "container start failed:\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    (container, port)
}
