# A Mac's sound over AirPlay

**Experimental.** This is a workaround for the Macs whose sound Screen Sharing
does not carry to remotex: `ard`, whose Standard mode has no measured audio path,
and `ard-virtual-display`, which does not negotiate High Performance's media
stream. Those Macs send their sound the way they send it to any speaker: the
gateway is an AirPlay 1 speaker on the LAN, the Mac picks it from its Sound output
menu, and what it plays reaches the browser like any other target's sound. An
`ard-high-performance` target never uses it: its sound comes with its picture on
the media stream, which mutes the Mac's own output while it runs. See
[Apple RFB 003.889](apple-vnc-889.md#the-media-stream-high-performances-picture-and-sound).

The speaker is `src/airplay/`. It began as a standalone proof of concept that was
measured against a physical Mac before any of it was wired in.

It is the `airplay` Cargo feature: on by default, and in every released
artifact, the container included. A build with `--no-default-features` and
without `--features airplay` has no speaker, refuses `[airplay]`, and carries no
sound from a Mac.

## Configuration

```toml
[airplay]
password = "choose one"

[[targets]]
name = "mac"
protocol = "vnc"
subtype = "ard"          # or "ard-virtual-display"
host = "mac.local"
username = "me"
password = "…"
```

The speaker is gateway-wide, and so is the switch: with `[airplay]`, every `ard`
and `ard-virtual-display` target carries audio, and without it none does. Every
Apple target refuses the `audio` key, and `[airplay]` with no Mac target at all
is refused. Beside an `ard-high-performance` target the table still loads, and
that target's sound still comes over its media stream. The
picker and the session's Info card say whether AirPlay is on for a Mac, and the
menu's **Enable AirPlay audio** button is shown only when it is. The table is top-level, like
`[branding]` and `[meter]`, so a `remotex tui` instance config may set it too. The
speaker is named `<[branding].text> - remotex`, which is what the Mac's Sound menu
shows. The name is one mDNS label, so `[branding].text` may be at most
40 bytes while `[airplay]` is set.

On the Mac, pick that name in Control Center → Sound, or in System Settings →
Sound → Output, and enter the password when asked. The Mac remembers the
password, so it is asked once. The choice lasts one session: when the session
ends the speaker hangs up, and the speaker is picked again for the next one.
`audio_codec`, `audio_bitrate`, `audio_adaptive` and `audio_adaptive_min` apply as
on any other target.

## What it is

- **AirPlay 1 (RAOP), not AirPlay 2.** The gateway advertises `_raop._tcp` over
  mDNS from inside the process, with no avahi or Bonjour on the host. It answers
  RTSP, and it receives Apple Lossless over RTP, in AES-128-CBC under a key the
  Mac wraps with RSA-OAEP. The RSA key is the one every AirPlay 1 receiver shares,
  extracted from the AirPort Express, as shairport-sync ships it. It proves
  nothing about the gateway: it only has to be used.
- **The password is RTSP Digest**, realm `raop`, checked on every request until
  one answers it. A Mac is asked on its first connection and remembers it after.
  It keeps other Macs on the LAN from playing into the session. It encrypts
  nothing that is not already encrypted.
- **One speaker per gateway, one stream per session.** The speaker stays up and
  advertised with the gateway, and feeds the audio bridge of whichever Apple
  session is running: each engine's start attaches its own bridge. A stream
  belongs to the session it was set up under. When that session ends, by
  disconnect, takeover or the engine exiting, though not when a browser briefly
  detaches, the speaker closes the Mac's connection, and the Mac takes its sound
  back to its own output. With no session running, a SETUP gets `453`. A
  new session does not win the Mac back: it has to pick the speaker again.
- **One sender at a time.** A second Mac's SETUP gets `453` until the first
  leaves.
- **44.1 kHz 16-bit stereo**, the only format a Mac sends, so `pcm` passthrough
  carries it at 1.41 Mbit/s like RDP's.

## What it does not do

- **Sync with video on the Mac.** An AirPlay sender expects the speaker to
  buffer about two seconds, and a Mac video player delays its picture to match.
  The gateway plays each packet as it arrives, since a remote desktop wants its
  sound now. So a video playing on the Mac is heard about two seconds before it
  is seen. Everything else, from system sounds to music to calls, is immediate.
- **Resend what the network drops.** A lost packet is a gap, as it is on every
  other path to the browser.
- **Follow the Mac's volume.** An AirPlay 1 sender leaves volume to the speaker,
  and this one leaves it to the browser.

## Network

- **The Mac must be on the gateway's link.** mDNS is link-local multicast, and
  macOS has no way to add an AirPlay speaker by address. A gateway that reaches the
  Mac over a VPN or a routed hop is never offered in its Sound menu, unless an mDNS
  reflector carries the service across.
- **The ports are ephemeral.** The RTSP port is whatever the OS picks, and the
  advertisement carries it. Each stream opens three more UDP ports, whose numbers
  the Mac learns from the RTSP answer. A host firewall has to let the Mac reach
  the gateway on TCP and UDP from the LAN.
- **A container needs the host's network** (`--network host`) for both the
  multicast and those ports. A bridged container's speaker is never seen.
- **Only routable addresses are advertised.** Every link-local IPv6 address is
  in `fe80::/64`, so a host with many interfaces — a Kubernetes node's veth per
  pod — would otherwise hand the Mac one per interface, scoped to the Mac's own
  link, where only one answers; the Mac tries one and reports it could not
  connect. The speaker leaves them out, so the gateway's link needs an IPv4
  address or a routable IPv6 prefix.
