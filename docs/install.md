# Installing remotex

Pre-built binaries for **Linux** (x86_64, arm64) and **macOS** (Apple Silicon).
Each release ships a self-contained tarball — the binary plus its frontend,
served from disk.

## Quick install

```bash
curl -fsSL https://andrewtheguy.github.io/remotex/install.sh | bash
```

This:

1. Detects your OS/arch and resolves the latest release.
2. Downloads the matching tarball and **verifies its SHA-256** against the
   digest GitHub publishes for the asset.
3. Installs under `/opt/remotex` and links a `remotex` launcher onto your
   `PATH` at `/usr/local/bin/remotex`. You may be prompted for `sudo` to write
   under `/opt` and `/usr/local/bin`.

> Review the script before piping it to a shell:
> <https://andrewtheguy.github.io/remotex/install.sh>

## Configure and run

```bash
# set your RDP target + credentials (kept server-side, never sent to the browser)
$EDITOR /opt/remotex/etc/remotex.toml

remotex serve
```

Then open the printed URL (default <http://127.0.0.1:52380>). The TOML config
format is documented in the [README](../README.md#configuration); all
configuration lives in that file (no environment variables). Pass
`--config <path>` to use a different file and `--target <name>` to pick a
`[[targets]]` profile.

## Options

Install a specific release:

```bash
curl -fsSL https://andrewtheguy.github.io/remotex/install.sh | bash -s -- v0.1.0
```

Install to a custom, non-root location (no `sudo` needed):

```bash
curl -fsSL https://andrewtheguy.github.io/remotex/install.sh \
  | PREFIX="$HOME/.local/opt/remotex" BINDIR="$HOME/.local/bin" bash
```

Make sure `BINDIR` is on your `PATH`.

## Layout

```
/opt/remotex/
├── etc/remotex.toml                       # stable user configuration
├── versions/<version>/{bin,share}       # this version's files
├── current -> versions/<version>        # active version
└── /usr/local/bin/remotex -> /opt/remotex/current/bin/remotex
```

The example config is versioned at
`current/share/doc/remotex/remotex.toml.example`. The binary resolves `share/`
relative to its own real path and the stable config relative to the enclosing
prefix, so the whole tree is relocatable via `PREFIX`/`BINDIR`.

## Upgrades and rollback

Re-running the installer stages the new version, flips the `current` symlink
atomically, and keeps the **immediately previous** version for rollback. Your
`etc/remotex.toml` remains untouched across upgrades and rollbacks.

Roll back by repointing `current` at the previous version:

```bash
ln -sfn versions/<previous> /opt/remotex/current
# e.g. ls /opt/remotex/versions  to see what's kept
```

## Uninstall

```bash
# whole install (all versions + config):
curl -fsSL https://raw.githubusercontent.com/andrewtheguy/remotex/main/packaging/uninstall.sh | bash

# or, from a checkout:
sudo bash packaging/uninstall.sh            # remove everything
sudo bash packaging/uninstall.sh 0.1.0      # remove a single version
```

## Building the tarball yourself

See [`packaging/README.md`](../packaging/README.md).
