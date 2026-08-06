# Known issues

Faults that are reproducible, understood well enough to recognise, and not
understood well enough to fix. Each entry says what it looks like, what has been
ruled out, and what would move it — so hitting one costs a lookup rather than an
investigation.

An issue leaves this file in one of two ways: it is fixed, or it turns out to be
something remotex is doing wrong, in which case it becomes work rather than a
note.

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
