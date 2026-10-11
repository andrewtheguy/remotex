# The browser's microphone over VNC with wlshare

How the browser's microphone reaches a wlroots-based Wayland desktop behind
wlshare, on the RFB connection the session already has, so a `wlshare` target can
take a microphone the way an RDP one does over MS-RDPEAI. The audio extension
([`wlshare-audio.md`](wlshare-audio.md)) carries sound from the server only, so
this private extension is the camera's twin
([`wlshare-camera.md`](wlshare-camera.md)). A `wlshare` target with
`microphone = true` lists a pseudo-encoding and wlshare answers that it takes a
microphone; until it answers, the browser's microphone is never plugged.

The server side is [wlshare](https://github.com/andrewtheguy/wlshare), which offers
the PCM to the desktop as a PipeWire `Audio/Source` node, "wlshare remote
microphone", that applications record from like any other. How wlshare builds that
node is in its own
[`docs/architecture.md`](https://github.com/andrewtheguy/wlshare/blob/main/docs/architecture.md#the-microphone-extension).

## Configuration

```toml
[[targets]]
name = "workstation"
protocol = "vnc"
subtype = "wlshare"
host = "127.0.0.1"
port = 5900
microphone = true
```

`subtype = "wlshare"` says the server is wlshare, and `microphone = true` is what
makes the gateway list the extension to it. The key is accepted on a `wlshare`
target and refused on a plain `vnc` target and on both Apple subtypes. As on RDP
it is capability only: the microphone is plugged when a browser enables it from the
floating menu, per session and never remembered, and unplugged when that socket
closes, the session changes hands, or the engine ends. wlshare's own `microphone`
key (default `false`) is the server's side of the same switch.

## The wire

The extension is wlshare's, and its messages and their layouts are in wlshare's
own [`docs/architecture.md`](https://github.com/andrewtheguy/wlshare/blob/main/docs/architecture.md#the-microphone-extension):
the pseudo-encoding `WLSM` and message type `0xE3` in both directions. The
gateway lists `WLSM` in `SetEncodings` after the camera request and ahead of the
density and outputs requests, which stay last.

wlshare answers *every* `SetEncodings` that lists the encoding with *available*,
and only the first is news. The format is the server's to name, as a host's is over
RDP; wlshare names mono at 48 kHz, which is the rate the gateway's Opus decodes at,
so the bridge resamples nothing. wlshare ends the session over a sample past
256 KiB or one that is not whole frames of that format. A server operation this
client does not know is fatal: the messages share no length field.

## The gateway's half

`src/vnc_mic.rs` adapts the mic socket's `MicBridge` to the VNC engine. It reuses
the same browser-facing socket, bridge, Opus decoder and encoder as MS-RDPEAI;
only the engine-side protocol differs. For wlshare, attaching the mic socket is a
device plug and closing it is an unplug. RDP does not need those messages because
its redirected recording device exists for the entire session.

Plug and unplug commands use an unbounded control queue. PCM uses a separate queue
of sixteen 20 ms buffers; when it is full, the oldest buffer is removed so the
stream resumes near the live edge. The engine always takes a pending control
command before PCM. An unplug clears queued PCM before its command is queued; a
recording reset clears it when Stop closes the bridge stream. A replacement plug
closes the active recording in the wire state but does not itself empty the PCM
queue. The engine loop writes these client messages, while its read loop converts
wlshare Start and Stop messages to the bridge's `micOpen` and `micClose` signals.

What reaches the wire is `vnc_mic::Device`'s decision, taken under the uplink so two
decisions leave in the order they were made:

- Before wlshare announces availability, a browser plug is retained but no wire
  message or PCM is sent. The announcement sends that pending plug.
- An unplug is sent only if the plug was announced to wlshare.
- A Start message opens the bridge only while the device is both announced and
  plugged. A Start already in flight when the browser unplugs is ignored.
- PCM is sent only after an accepted Start and before Stop.
- Unplugging or replacing a microphone ends an active bridge recording immediately,
  because wlshare removes the PipeWire node without sending a separate Stop. Plug,
  unplug, Start and Stop update the `Device` under the same mutex, so their state
  tests and resulting bridge notifications are ordered together.

## What is checked

Both halves' wire formats are unit tested against independent decoders — this
side's in `src/vnc_mic.rs`, wlshare's in `crates/wlshare-rfb/src/microphone.rs` —
and so are the device's rules, the queues, and the bridge keeping a plug made
before the engine registers.

The container test in `tests/wlshare_e2e.rs` follows the whole path but the page:
wlshare on a headless sway with PipeWire and WirePlumber, and the gateway, driven by
a client that opens the mic socket as the session starts and sends 60 ms Opus
packets of a 440 Hz tone. It asserts that wlshare lends the desktop a node, that
`pw-record` linking to it sends `micOpen` and leaving it sends `micClose`, that what
was recorded is loudest at 440 Hz, and that closing the socket removes the node.

Measured 2026-09-14 against wlshare on a labwc session with PipeWire 1.4.2,
without the gateway: a probe client speaking the extension plugged a microphone
and sent a 440 Hz tone while `pw-record --target wlshare-microphone-1` recorded.
wlshare sent start as the recorder linked and stop as it left, and the recording
held the tone without gaps past the node's 60 ms prefill. The path through the
gateway and a browser is checked by hand.
