# Packaging

The gateway ships as a relocatable tarball containing the Rust binary, built
frontend, config example, and version metadata.

## Installed layout

```text
<prefix>/
├── etc/remotex.toml
├── versions/<version>/
│   ├── bin/remotex
│   ├── share/doc/remotex/remotex.toml.example
│   ├── share/remotex/web/
│   └── VERSION
├── current -> versions/<version>
└── .install.lock
```

The launcher points to `<prefix>/current/bin/remotex`. The binary resolves its
assets relative to its real path and loads the stable config from
`<prefix>/etc/remotex.toml`.

## Scripts

| Path | Purpose |
|---|---|
| `build-tarball.sh` | build the gateway and assemble a platform tarball |
| `install.sh` | stage a version, switch `current`, and retain one rollback |
| `uninstall.sh` | remove an installation or one version |
| `Dockerfile` | build an image from an extracted release tarball |

remotex.app is not built here. It is an Electron shell in
[`apps/viewer`](../apps/viewer) — `bun run dist` there — and it carries a copy of
the gateway this directory packages. See [`docs/macos-viewer.md`](../docs/macos-viewer.md).

The repository-root `install.sh` downloads and verifies a release before
calling `packaging/install.sh`.

## Local build

```sh
cd frontend && bun install --frozen-lockfile && cd ..
bash packaging/build-tarball.sh
```

Output is written to `dist/remotex-<version>-<os>-<arch>.tar.gz`.

## Releases

`.github/workflows/release.yml` creates a draft, builds gateway tarballs for
Linux x86_64, Linux arm64, and macOS arm64, then publishes only after all assets
succeed.

The frontend is built once and reused for every platform. Container images are
assembled from the published Linux tarballs, so they contain the same binary
and frontend as the release assets.
