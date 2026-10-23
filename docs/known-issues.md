# Known issues

Reproducible limitations and defects that a user may encounter in a supported
configuration. Each entry describes the symptom, the evidence collected so far,
and the available workaround or condition that changes it. Entries can cover the
remote server, the browser, or zDesk itself; knowing which component is at fault
does not make the user-visible issue disappear.

Remove an entry when the symptom is fixed or no longer applies to a supported
configuration. If a confirmed zDesk defect has separate tracking, keep the
user-facing description here until the fix ships, with a link to that tracking
when it is public.

## Blurry text after dragging the window to a new size, on an RDP host's graphics pipeline

**What it looks like.** In an RDP session started with resize, whose host draws
through the graphics pipeline
([MS-RDPEGFX](rdp-client.md#the-graphics-pipeline-ms-rdpegfx)), dragging the
browser window's edge to resize it can leave the desktop's text blurry. It does
not clear on its own: the text stays blurry until the connection to the host is
ended and made again.

**What has been ruled out.** That it is zDesk's. Microsoft's own Remote Desktop
client on a Mac, the Windows App, shows the same blurry text when its window is
dragged to a new size against the same kind of host, so the picture is what the
host sends for a desktop resized that way and not something this client's
decoders or the page make of it.

**What would move it.** Only a new connection to the host is known to: the
gateway's to it, started again from the picker, as Microsoft's client has to
disconnect and reconnect its own. It is recorded so that blurry text after a
resize is recognised as the host's rather than investigated as a decoder or
scaling fault in the gateway or the page.

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

**What has been ruled out.** That it is zDesk's. Apple's own Screen Sharing
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
