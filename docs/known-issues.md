# Known issues

Faults that are reproducible, understood well enough to recognise, and not
understood well enough to fix. Each entry says what it looks like, what has been
ruled out, and what would move it — so hitting one costs a lookup rather than an
investigation.

An issue leaves this file in one of two ways: it is fixed, or it turns out to be
something remotex is doing wrong, in which case it becomes work rather than a
note.

## Blurry text after dragging the window to a new size, on an RDP host's graphics pipeline

**What it looks like.** In an RDP session started with resize, whose host draws
through the graphics pipeline
([MS-RDPEGFX](rdp-client.md#the-graphics-pipeline-ms-rdpegfx)), dragging the
browser window's edge to resize it can leave the desktop's text blurry. It does
not clear on its own: the text stays blurry until the connection to the host is
ended and made again.

**What has been ruled out.** That it is remotex's. Microsoft's own Remote Desktop
client on a Mac, the Windows App, shows the same blurry text when its window is
dragged to a new size against the same kind of host, so the picture is what the
host sends for a desktop resized that way and not something this client's
decoders or the page make of it.

**What would move it.** Only a new connection to the host is known to: the
gateway's to it, started again from the picker, as Microsoft's client has to
disconnect and reconnect its own. It is recorded so that blurry text after a
resize is recognised as the host's rather than investigated as a decoder or
scaling fault in the gateway or the page.

## A veiled rectangle over a playing video, on an RDP pipeline passed with H.264

**What it looks like.** In an RDP session whose pipeline is passed through, on a
target with the experimental `egfx_h264` key
([RDP's graphics pipeline, passed through](architecture.md#rdps-graphics-pipeline-passed-through)),
a rectangle over most of a playing video, or of anything else on the desktop that
moves like one, looks as if a translucent sheet lay on it: fine detail inside it
is smeared into flat blocks, while a margin of the same video outside it stays
sharp. The rectangle can appear in one place and then another before it settles,
and it stays for as long as the video plays.

**What has been ruled out.** That it is the page's decoding or compositing. The
rectangle is the region the host draws with H.264, its edges on the host's own
grid rather than the video's, and what is outside it is drawn with the lossless
codecs, which is why the edge shows. The host says with each access unit how hard
it quantized it. On the content this was seen with, a fine halftone pattern
animated in a browser on a Windows 11 host with no GPU at 1920×1080, nearly every
unit carried the coarsest of the three settings that host has been seen to use
(QP 41, quality 38). A screenshot of the page, in Chrome with its software
decoder, matched FFmpeg's decode of the same access unit, and opening and closing
the menu over the session changed nothing in the stream.

**What would move it.** Taking `egfx_h264` off the target: its pipeline is then
lossless, and the video is sharp at the lossless codecs' cost in bytes. Whether
the host can be led to a finer setting is not known. It chose a finer one for
most of a film clip at 1280×800, so content and size move it; nothing has been
tried from the client's side, such as answering the host's network detection,
which this client does not. It is recorded so that the rectangle is recognised
as the host's encoding rather than investigated as a fault in the page's
decoders or the compositor.

## The cursor stays the arrow while a selection tool is up, on a Mac

**What it looks like.** In a session to a Mac, a tool that takes over the screen
to let the user pick something shows its own cursor on the Mac and the old one in
the session. Two have been seen:

- Shift-Command-4 and then Space, to take a screenshot of a window: the Mac's
  cursor becomes a camera, the session's stays the arrow.
- A screen recording started in QuickTime Player, while choosing what to record:
  the same camera on the Mac, the same arrow in the session.

The tool still works in both: a click takes the screenshot or starts the
recording. Cursors that change on hover, such as the hand over a link, are
unaffected.

**What has been ruled out.** That it is remotex's. Apple's own Screen Sharing
viewer keeps the arrow in both cases too. The Mac's cursor changes as soon as the
tool starts, and the Mac sends no cursor shape for as long as the tool is up: the
camera goes out only when the tool ends, with the ordinary shape right behind it,
so there is nothing for a client to draw in between. See the cursor cache under
[The record layer](apple-vnc-889.md#the-record-layer).

**What would move it.** Nothing known: no message a viewer sends makes the Mac
send the shape sooner. Other tools of the same kind may do it and have not been
tried; for one, check Apple's viewer first, and add it to the list above if the
cursor stays the same there too. It is recorded so that a cursor that does not
change during a screenshot or a recording is recognised as the Mac's rather than
investigated as a fault in the gateway's cursor handling or the page. It was
measured in Standard mode; High Performance has not been tried.
