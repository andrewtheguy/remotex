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

### Cleanups bypass the payload bound at 2×

`Regions::due` (`src/regions.rs`) returns runs of whole grid cells, and
`flush_cleanups` (`src/encode.rs`) hands them to `encode_tile` directly. At 2× a
cell is 128 pixels tall, so a cleanup is twice `BAND_ROWS` — the bound that exists
so one record stays inside `MAX_BATCH_BYTES` and inside the slot cache's ceiling
(`src/wire.rs`). A full-width 2× cleanup is 3840×128.

The motion path already does this correctly: it bands first and cuts at the grid
only inside a band, which is why a piece of a cell keying to that cell is spelled
out in `BAND_ROWS`'s comment. The cleanup path is the one place that skipped it.

Wants an integrated 2× cleanup test; there is none.

### `Shadow::accept` re-compares a cell once per row it spans

The classification loop in `Shadow::accept` (`src/tiles.rs`) walks the cells of each
row's differing span and `memcmp`s each one. A cell is 64 rows tall, so a cell
already known to differ is compared up to 63 more times for an answer that cannot
change, and `cells` is pushed to once per row rather than once per cell — a
full-screen 1080p change reaches roughly 32,000 entries before the `sort_unstable`
and `dedup`, against about 510 distinct answers.

At 320 wide both numbers were a fifth of that, which is why this was never worth
saying. A column bitmap for the current cell row, cleared when the row changes,
skips the comparison for a cell already marked and pushes each cell once.

`differing_bytes` still runs per row: the bounding box needs it, and it is a
`memcmp` over the whole row rather than per cell.

### The motion path packs every pixel twice

`TileSink::damage_streaming` (`src/encode.rs`) calls `pack(changed.rect)` to blit
the mirror, then `pack(band)` or `pack(run)` again for what it sends. `pack` is a
fresh `Vec` and a `tiles::crop` per call (`src/vnc.rs`, `src/rdp.rs`), so the
rectangle is copied twice per damage report. The split path went from at most six
crops per 1080p band to at most thirty, each with its own allocation.

Packing `changed.rect` once and cropping sub-rectangles out of that buffer removes
the second pass and the per-run allocation. The callback stays a callback for the
reason `damage` gives — the two engines hold their pixels differently — it is only
called once.

### `streamed` is built when no stream is live

`damage_streaming` hashes every cell of the changed rectangle and looks it up in
`covered` before the band loop, whether or not any region exists. On a
`render_motion` target with nothing playing — the ordinary case for a text desktop —
that is now about 510 lookups per damage report to build an empty set. An empty
`covered` means the quiet path for every band, and can be read before the walk.

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
