# Known issues

Faults that are reproducible, understood well enough to recognise, and not
understood well enough to fix. Each entry says what it looks like, what has been
ruled out, and what would move it — so hitting one costs a lookup rather than an
investigation.

An issue leaves this file in one of two ways: it is fixed, or it turns out to be
something remotex is doing wrong, in which case it becomes work rather than a
note.

## remotex.app: H.264 does not decode

`render_type = "video"` (and `render_motion_subtype = "h264"` under `mixed`) plays
in a browser and not in `remotex.app`. What the user sees is the client's own
banner — "this browser cannot decode…" — over a desktop that never paints.

Unlike everything else in this file, the cause is not open: the app embeds
Chromium, and the CEF binaries it is built from are the stock ones, which ship
**without proprietary codecs**. `VideoDecoder.isConfigSupported('avc1…')` answers no,
and the client reports that the way it reports any unsupported codec. Nothing is
wrong with the stream, the gateway, or the client.

It is here rather than fixed because the fix is not a change to remotex: it is
building Chromium with `proprietary_codecs=true` and `ffmpeg_branding=Chrome`,
which is hours of build for a licence question that has not been answered. The app
holds no codec logic of its own and must not grow any — no check, no gate, no
fallback path. A `video` target failing in the app looks exactly like the same
target failing in a browser without the codec, which is the right shape for it.

What would move it: either a CEF build with the codecs, or VP9/AV1 as a render
subtype — every engine has those, and it is on the roadmap.


## RDP: the reactivation a size change triggers can fail

When a resize actually changes the desktop size, the server answers
`DeactivateAll` and `reactivate` runs the Deactivation-Reactivation Sequence.
That sequence sometimes fails at the first PDU it reads, ending the session with

```text
RDP session ended: reactivation: … invalid `pdu_type`: invalid pdu type
```

decoding a `ShareControlHeader`, or occasionally `read frame: cannot decrypt peer's
message`. Both are stream-level: what arrives is not the PDU the sequence expects.

**It is not tied to one machine.** Old and current hosts both fail it; what
differs is how often. Forcing a real size change:

| host | result |
|---|---|
| 2013 MacBook Pro running Windows | fails most attempts (12 in ~18) |
| a current Windows desktop | fails occasionally |

Speed plausibly explains that rate — a race the slow machine loses most of the
time is one the fast machine loses sometimes — but not the failure itself, and the
cause is open.

Ruled out: it reproduces identically on `fixed-quality`/`webp` and on `video`, so
neither encoder is implicated, and it predates the render dial entirely. Both VNC
resize paths are unaffected.

One thing that makes it look intermittent from a browser: only a size change that
is *real* reactivates at all. Asking twice for the same size triggers it once, and
a request equal to the current size never triggers it.

**How often it can be reached is bounded.** Clients may resize an RDP target only
when the user asks; the window is never allowed to drive the size continuously,
which is a permission the gateway withholds (`TargetConfig::auto_resize`). That is
containment, not a fix — one "Resize to window" can still land on this — but a
drag that used to walk into it repeatedly now cannot.

What would move it: a packet capture of a failing sequence, to say what actually
arrived where the expected PDU should have been.


## Apple High Performance: a resize can leave the screen wrong

`subtype = "ard-high-performance"` with `resize = true` renegotiates the virtual
display on every viewport report: remotex resends the full
`SetDisplayConfiguration` descriptor with a replacement mode, and the Mac's
answering `AppleDisplayLayout` sets the actual framebuffer geometry. That
exchange does not always settle. When it doesn't, the desktop is left wrong and
stays wrong for the rest of the session; reconnecting is the way out, because a
fresh session builds the virtual display during setup rather than from a
replacement mode.

**This one is less characterised than the fault above, and says so.** There is no
error to match: nothing fails, the session continues, and the picture is simply
not the one that was asked for. It has not been pinned to a size, a direction of
change, or a rate.

What is certain is its scope: it takes the 003.889 path and it takes `resize =
true`. Standard `ard` refuses resize outright, so no Apple Standard session can
reach it, and the resize paths of generic `vnc` and RDP share none of this
machinery.

What to suspect first is the descriptor itself. Apple documents none of this
revision, so every field in it is measurement rather than specification, and two
are known unknowns: the `+0x96` rotation value, whose private bits are unread,
and a layout payload two bytes shorter than its own length prefix claims — see
[`apple-vnc-889.md`](apple-vnc-889.md).

**How often it can be reached is bounded**, the same way the RDP fault above is:
the window is never allowed to drive this target's size, so a resize is something
the user asks for once and can connect to what follows. Again containment rather
than a fix.

What would move it: the descriptor sent and the layout the Mac answers with,
captured across a resize that goes wrong, to say whether the Mac declined the
requested mode or remotex misread the answer it gave.
