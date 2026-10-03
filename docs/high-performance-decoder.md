# High Performance decoder

An `ard-high-performance` target sends its picture as HEVC and its sound as
AAC-ELD. The sound goes to the browser as the Mac sent it, and the browser
tries to decode it; a browser that cannot still runs the session without sound
and says why in the Audio row. The picture the gateway decodes, to send as VP9,
with a library that is in no release artifact, because its licence keeps it
out. Install it on the host that runs the gateway:

| Library | Decodes | Version | Licence |
|---|---|---|---|
| FFmpeg's libavcodec, with its libavutil | the HEVC picture | FFmpeg 6.1 to 9 (libavcodec 60 to 63) | LGPL-2.1-or-later |

Only `ard-high-performance` needs it. `rdp`, plain `vnc` and `ard` targets,
`virtual_display = true` included, do not.

The gateway loads it when a session needs it, and looks again at every session
until it finds it. So it can be installed while the gateway runs: the next
session uses it. A Windows gateway whose config names its folder loads it from
there when it starts instead: see
[Installed somewhere else](#installed-somewhere-else).

- [Debian](#debian)
- [Ubuntu](#ubuntu)
- [Other Linux distributions](#other-linux-distributions)
- [macOS](#macos)
- [Windows](#windows)
- [Container](#container)
- [Checking the install](#checking-the-install)
- [Without the decoder](#without-the-decoder)

## Debian

On Debian 13 (trixie):

```sh
sudo apt update
sudo apt install libavcodec61
```

The `.deb` recommends it, so installing the `.deb` brings it in and this step
is not needed.

## Ubuntu

On Ubuntu 24.04:

```sh
sudo apt update
sudo apt install libavcodec60
```

It is in `universe`, which a stock install has on
(`sudo add-apt-repository universe` turns it on where it is off). Later
releases name the package after FFmpeg's own major: `libavcodec61` on 25.04 and
25.10. The `.deb` recommends whichever the release has, as on Debian.

## Other Linux distributions

Install the distribution's FFmpeg libraries. The gateway asks the system loader
for these names, so any package that provides them serves:

| Library | File |
|---|---|
| libavcodec | `libavcodec.so.60` to `libavcodec.so.63` |
| libavutil | `libavutil.so.58` to `libavutil.so.61`, the one released with that libavcodec |

## macOS

With [Homebrew](https://brew.sh):

```sh
brew install ffmpeg
```

The gateway finds it in Homebrew's `lib` (`/opt/homebrew/lib`, or
`/usr/local/lib` on an Intel Mac) with nothing to configure. It also looks in
MacPorts' `/opt/local/lib` and wherever the system loader does, for
`libavcodec.60.dylib` to `libavcodec.63.dylib`.

On macOS the loaded libavcodec decodes through VideoToolbox.

## Windows

The step below is the recommended install, not the only one. Any shared build
of FFmpeg 6.1 to 9 serves, from wherever it is installed: the gateway finds the
recommended install by itself, and `[hp_decoders]` tells it where any other is.
See [Installed somewhere else](#installed-somewhere-else).

Recommended: a shared build of FFmpeg 9.0 from winget, a download of about
80 MB, from PowerShell 7 (`pwsh`):

```powershell
winget install BtbN.FFmpeg.LGPL.Shared.9.0
```

winget puts the build's `bin` folder, which holds `avcodec-63.dll` and
`avutil-61.dll`, on the `PATH` of the user who installed it. So run the gateway
as that user, from a shell opened after the install. A gateway already running
has to be restarted to see the new `PATH`.

### Installed somewhere else

FFmpeg installed any other way, or in any other folder, works too. Tell the
gateway where it is with `[hp_decoders]` in its config. That covers a shared
build unzipped by hand and a gateway run by another user than the one winget
installed for.

```toml
[hp_decoders]
ffmpeg_dir = 'C:\ffmpeg\bin'
```

| Key | The folder holds |
|---|---|
| `ffmpeg_dir` | `avcodec-60.dll` to `avcodec-63.dll`, the `avutil-58.dll` to `avutil-61.dll` released with it, and the other DLLs of that build: a shared build's `bin` |

It is a whole path, drive included.

The named folder is the only place FFmpeg is loaded from, and the gateway loads
it when it starts: one that does not find the decoder there refuses to start,
and says which file it tried. Only a gateway on Windows takes this table.

### Where the gateway looks by default

Without `[hp_decoders]`, the gateway looks when a session needs the decoder, by
Windows' own search: beside `remotex.exe`, then the folders on `PATH`, which is
what finds the recommended install.

## Container

The public image, `ghcr.io/andrewtheguy/remotex`, does not have the library.
To decode in a container, build an image on top of it that adds Debian's:

```dockerfile
FROM ghcr.io/andrewtheguy/remotex:latest
RUN apt-get update \
    && apt-get install -y --no-install-recommends libavcodec61 \
    && rm -rf /var/lib/apt/lists/*
```

## Checking the install

Connect to the `ard-high-performance` target from a browser. When the gateway
decodes the picture, its log names the library it loaded:

```text
vnc: the HEVC decoder is libavcodec 61.19.101, from libavcodec.so.61
```

A Windows gateway with `[hp_decoders]` logs the same line when it starts.

When the library is missing, the page shows the reason and the log repeats it,
naming the library, how to install it, and every file it tried:

```text
vnc: refusing a session that does not pass the Mac's picture: the HEVC decoder, FFmpeg's libavcodec, is not installed: …
```

## Without the decoder

A gateway without it still serves an `ard-high-performance` target, with the
picture passed through, when the page's startup probe finds an HEVC decoder
path, either native or the optional software decoder described in
[`remotex.example.toml`](../remotex.example.toml). Support depends on the
browser, platform and GPU, so the picker is the answer rather than a
browser-name table. It shows the passthrough as already chosen there. The HEVC
goes to that browser as the Mac sent it, and nothing is decoded on the gateway.
Without a decoder path the target's Start is greyed, with the reason, so the Mac
is never dialled.

The sound needs nothing on the gateway either way: every session is sent the
Mac's AAC-ELD, which the browser tries to decode. A browser that cannot decode
it plays the session without sound and says so under Audio in the menu.

See [The media stream](apple-vnc-889.md#the-media-stream-high-performances-picture-and-sound)
for what the stream carries, and
[Prebuilt native dependencies](../packaging/README.md#prebuilt-native-dependencies)
for why no artifact links the library and for the `apple-hp-media-static` build
that does.
