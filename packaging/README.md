# Packaging

Distro-agnostic tarball packaging for Linux and macOS. rdpweb ships as a single
binary plus its built frontend (served from disk, not embedded), laid out under
a relocatable prefix.

## Layout after install

```
/usr/local/opt/rdpweb/
├── versions/
│   └── <version>/
│       ├── bin/rdpweb            # the binary
│       ├── etc/rdpweb.env        # config (migrated across upgrades)
│       ├── share/rdpweb/web/     # built frontend (index.html + assets)
│       └── VERSION
├── current -> versions/<version> # active version (atomic rename swap)
└── .install.lock                 # present only while an install runs

/usr/local/bin/rdpweb -> /usr/local/opt/rdpweb/current/bin/rdpweb
```

The binary resolves its `share/` and `etc/` **relative to its own real path**
(`current_exe()` canonicalized through the symlinks), so the tree can live
anywhere via `PREFIX` / `BINDIR`. `--static-dir` / `RDPWEB_STATIC_DIR` and
`RDPWEB_ENV_FILE` override the defaults.

## Files

| File | Purpose |
|------|---------|
| `build-tarball.sh` | Build the frontend + release binary and assemble `dist/rdpweb-<version>-<os>-<arch>.tar.gz`. Run once per target platform (no cross-compile). |
| `install.sh` | Lay an extracted tarball down under `PREFIX`: stage → atomic `current` swap → prune to (new + previous). Locked against concurrent runs. |
| `uninstall.sh` | Remove the whole prefix, or a single version (`uninstall.sh <version>`). |
| `etc/rdpweb.env.sample` | Config template; seeded to `etc/rdpweb.env` on a fresh install. |

The repo-root `install.sh` is the network installer (`curl … | bash`): it
downloads the right tarball, verifies its SHA-256 against GitHub's published
digest, extracts, and invokes this `packaging/install.sh`.

## Build locally

```sh
cd frontend && bun install --frozen-lockfile && cd ..
bash packaging/build-tarball.sh
# -> dist/rdpweb-<version>-<os>-<arch>.tar.gz
```

## Upgrades & rollback

Each install keeps the previous version. Roll back by repointing `current`:

```sh
ln -sfn versions/<previous> /usr/local/opt/rdpweb/current
```

## Releasing

`.github/workflows/release.yml` (manual `workflow_dispatch`) builds the three
tarballs (linux x86_64, linux arm64, macOS arm64) and publishes a `v<version>`
GitHub release. `.github/workflows/pages.yml` serves `install.sh` from GitHub
Pages.
