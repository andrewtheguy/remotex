# Development

## Running from a checkout

Install the frontend dependencies once, then use Cargo. `cargo run` rebuilds
the frontend when its sources change and compiles the generated bundle into the
gateway binary.

```sh
bun install --cwd frontend
cp remotex.example.toml remotex.toml
cargo run -- gen-passwd admin
# Paste the generated credential into remotex.toml, then:
cargo run -- serve -c remotex.toml
```

Open <http://localhost:52380>. Use `RUST_LOG=info` or `RUST_LOG=debug` for
backend logs. Use `cargo build` when you only need to compile without starting
the gateway.

The built frontend is embedded in the binary at compile time (`src/assets.rs`),
so `target/release/remotex` runs on its own with no `frontend/dist` beside it,
and the gateway is the only thing that serves the page. `build.rs` runs
`bun run build` into Cargo's private output directory before the crate compiles
and fails the build if `index.html` is missing afterwards. The frontend's build
compiles the page's WebAssembly modules too; see
[Local build](../packaging/README.md#local-build) for the required toolchain.

The main directories are:

| Path | Contents |
|---|---|
| `src/` | gateway, session management, and RDP/VNC engines |
| `src/rdp_client/` | the RDP client, down to the wire format |
| `crates/remotex-rdp-graphics/` | the RDP graphics pipeline's codecs and compositor, which the page runs too |
| `frontend/` | React SPA, and its WebAssembly modules under `frontend/wasm/` |
| `tests/` | protocol and engine end-to-end tests |
| `packaging/` | release, install, and container scripts |

[Architecture](architecture.md#backend) names each backend module.

## Checks

```sh
cargo clippy --all-targets -- -D warnings
cargo test --lib

cd frontend
bun run check
cd ..
```

## End-to-end tests

The container-backed VNC and wlshare tests use Docker or Podman and do not
start a browser. They are ignored by default; run them explicitly with:

```sh
cargo test --test vnc_e2e --test wlshare_e2e -- --ignored
```

`wlshare_e2e` builds wlshare from the `../wlshare` checkout, or the one
`REMOTEX_TEST_WLSHARE_DIR` names. `REMOTEX_TEST_CONTAINER_RUNTIME` forces Docker
or Podman when both work.

For a remote Podman connection:

```sh
CONTAINER_CONNECTION=workstation-wsl \
REMOTEX_TEST_CONTAINER_HOST=<engine-host> \
cargo test --test vnc_e2e -- --ignored
```

`CONTAINER_CONNECTION` is the Podman system connection name.
`REMOTEX_TEST_CONTAINER_HOST` is the engine host's IP address or DNS name as
reachable from the machine running the tests; an SSH config alias is not
resolved for the tests' direct VNC connections.

RDP has no container to test against: the gateway's RDP client speaks NLA to a
current Windows host and nothing else, so its end-to-end tests borrow a real
machine — see [`tests/rdp_proto_probe.rs`](../tests/rdp_proto_probe.rs) and
[`tests/rdp_client_probe.rs`](../tests/rdp_client_probe.rs).

Stable headless browser checks for DOM/control-plane flows live under
[`tests/playwright`](../tests/playwright/README.md). They intentionally do not
assert framebuffer/canvas output, cursor rendering, or gesture timing.

## Build

```sh
bun install --cwd frontend
cargo build --release
bash packaging/build-tarball.sh
bash packaging/build-native-packages.sh
```

The native package builder consumes the tarball so every artifact contains the
same gateway binary. A release builder can name a platform-independent bundle
that it built earlier:

```sh
bun run --cwd frontend build
REMOTEX_PREBUILT_FRONTEND=frontend/dist cargo build --release
```

A gateway for QA is built with `cargo build --profile qa` into `target/qa`:
optimised as a release build is, without its link-time optimisation, so a
change rebuilds in seconds rather than minutes. Artifacts are always built
`--release`.

See [Packaging](../packaging/README.md) for the layouts, the prebuilt native
dependencies and the release workflow.
