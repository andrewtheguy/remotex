# Packaging

Distro-agnostic tarball packaging for Linux and macOS. remotex ships as a single
binary plus its built frontend (served from disk, not embedded), laid out under
a relocatable prefix.

## Layout after install

```
/usr/local/opt/remotex/
├── etc/
│   └── remotex.toml               # the global config (stable across versions)
├── versions/
│   └── <version>/
│       ├── bin/remotex            # the binary
│       ├── share/doc/remotex/
│       │   └── remotex.toml.example # versioned config example
│       ├── share/remotex/web/     # built frontend (index.html + assets)
│       └── VERSION
├── current -> versions/<version> # active version (atomic rename swap)
└── .install.lock                 # present only while an install runs

/usr/local/bin/remotex -> /usr/local/opt/remotex/current/bin/remotex
```

The binary resolves its versioned `share/` from its own real path
(`current_exe()` canonicalized through the symlinks), then loads the global
config from the enclosing prefix's `etc/remotex.toml`. Config is global-only —
no per-user or working-directory files; `--config <path>` is the sole
override. The tree can live anywhere via `PREFIX` / `BINDIR`.

## Files

| File | Purpose |
|------|---------|
| `build-tarball.sh` | Build the frontend + release binary and assemble `dist/remotex-<version>-<os>-<arch>.tar.gz`. Run once per target platform (no cross-compile). |
| `install.sh` | Lay an extracted tarball down under `PREFIX`: stage → atomic `current` swap → prune to (new + previous). Locked against concurrent runs. |
| `uninstall.sh` | Remove the whole prefix, or a single version (`uninstall.sh <version>`). |
| `etc/remotex.toml.example` | Config template installed under the active version's `share/doc/remotex/` and seeded to the stable `etc/remotex.toml` on a fresh install. |

The repo-root `install.sh` is the network installer (`curl … | bash`): it
downloads the right tarball, verifies its SHA-256 against GitHub's published
digest, extracts, and invokes this `packaging/install.sh`.

## Build locally

```sh
cd frontend && bun install --frozen-lockfile && cd ..
bash packaging/build-tarball.sh
# -> dist/remotex-<version>-<os>-<arch>.tar.gz
```

## Upgrades & rollback

Each install keeps the previous version. Roll back by repointing `current`:

```sh
ln -sfn versions/<previous> /usr/local/opt/remotex/current
```

## Releasing

`.github/workflows/release.yml` (manual `workflow_dispatch`) is draft-first:
it validates the Cargo.toml version (real TOML parse), aborts if the `v<version>`
tag already exists, creates a **draft** release up front (replacing any stale
draft from a failed run), builds the three tarballs (linux x86_64, linux arm64,
macOS arm64), uploads them to the draft, and only then publishes it — which is
what creates the tag, so a failed build never leaves a tag or a half-populated
release. `.github/workflows/pages.yml` serves `install.sh` from GitHub Pages.
