# Follow-ups from the 64×64 tile grid

The grid was cut from 320×64 to 64×64 pixels (`42545fd`) and then re-expressed in
points, so a 2× framebuffer cuts at 128×128 (`17a745c`). Five times as many cells
cover the same desktop, and every per-cell cost went with them. Nothing here is a
compatibility path — no consumer still assumes the old pitch — these are the places
where a cost that was tolerable at 320 wide is worth revisiting at 64.

Each item names the mechanism and the cost. An item that lands is deleted from this
file: the commit and its test are the record, and a done-list here would only
compete with them. What is left in this file is what is left to do.

## Open

### The motion path copies the changed rectangle twice

`TileSink::damage_streaming` (`src/encode.rs`) calls `pack(changed.rect)` to blit
the mirror, then `pack(band)` or `pack(run)` again for what it sends, so the
rectangle's pixels are copied twice per damage report where the plain tiles path
copies them once.

Cropping the bands out of that first pack instead of calling `pack` again saves
nothing, which is the trap this entry exists to name: `pack` is *already* a
`tiles::crop` on both engines (`src/rdp.rs`, `src/vnc.rs`), out of a buffer each
has already packed once, so the same rows are copied either way. What the finer
grid changed here is only the *number* of allocations — up to thirty crops per
1080p band where there were six — and not the bytes.

What would remove the second copy is packing each band once and blitting *that*
into the mirror, so the whole-rectangle pack goes away. It is not free.
`Regions::blit` stages every rectangle it is handed, and a full-screen damage
report would stage seventeen bands where it stages one, reaching `STAGED_CAP`
seventeen times sooner; past that the staged list collapses to a bounding box and
the spare mirror's sync copies slop the single blit did not. One fewer copy of the
changed rectangle against a coarser sync is a measurement, not an argument, and
neither side of it has a number yet.

## Watched, not planned

### `merge` is cubic in components

`merge` (`src/regions.rs`) is O(n²) per pass and removes one component per pass.
Components are bounded by moving cells over `MIN_STREAM_CELLS`, which at 1080p went
from about 20 to about 102 — roughly 130× the work per retune. It runs twice a
second on integers and is not worth changing at `RETUNE = 500 ms` and
`MAX_STREAMS = 4`. It is written down so it is not a surprise if either moves.

### `RETUNE` and `STREAM_IDLE` are calibrated against the old grid

Both are 500 ms, and the keyframe-waste measurement behind them is explicitly in
320×64 cells — see [`roadmap.md`](roadmap.md), which owns this question. The
figure does not carry across: a 64-point grid grows a region's bounding box at a
different rate, so the share of the stream spent on replaced rectangles has to be
measured again before either number moves.
