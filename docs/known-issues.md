# Known issues

Faults that are reproducible, understood well enough to recognise, and not
understood well enough to fix. Each entry says what it looks like, what has been
ruled out, and what would move it — so hitting one costs a lookup rather than an
investigation.

An issue leaves this file in one of two ways: it is fixed, or it turns out to be
something remotex is doing wrong, in which case it becomes work rather than a
note.

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

What would move it: a packet capture of a failing sequence, to say what actually
arrived where the expected PDU should have been.
