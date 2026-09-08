# Follow-ups from the 64×64 tile grid

The grid was cut from 320×64 to 64×64 pixels (`42545fd`) and then re-expressed in
points, so a 2× framebuffer cuts at 128×128 (`17a745c`). Five times as many cells
cover the same desktop, and every per-cell cost went with them. Nothing here is a
compatibility path — no consumer still assumes the old pitch — these are the places
where a cost that was tolerable at 320 wide is worth revisiting at 64.

Each item names the mechanism and the cost. An item that lands is deleted from this
file: the commit and its test are the record, and a done-list here would only
compete with them. What is left in this file is what is left to do.

## Watched, not planned

### The JPEG size floor is not scaled, and now equals one cell

`MIN_PHOTO_PIXELS = 4096` (`src/classify.rs`) is the area below which a tile is
never offered to JPEG. It is in pixels because what it guards is JPEG's fixed
header and table bytes, which do not shrink on a 2× desktop — so unlike the grid
it does not move with density, and a 2× tile clears it at a quarter of the screen
area a 1× tile needs.

It was chosen against the 320×64 grid, where it was a fifth of a cell and caught
only sub-cell damage. It now equals a 64×64 cell exactly, which is a coincidence
and not a link. The smallest whole-cell tile the encoder is handed is 64×64 =
4096 at 1×, which clears the floor by a single pixel, and 128×64 = 8192 at 2×,
because a band stays 64 rows at either density. Only a cell clipped by the
framebuffer's bottom edge falls under it — 64×56 on a 1080-tall desktop.

What actually drifts is the floor's second job. Keeping small sharp furniture out
of the lossy arm is a claim about points, and a 2× widget sliver four times this
size now reaches the content tests. The palette gate refuses flat chrome there,
so the exposure is narrow: colourful, gradient-heavy fragments that are neither
photographs nor text. Scaling the floor by density would fix the second job and
break the first, and there is no measurement saying the second one is failing.

### `merge` is cubic in components

`merge` (`src/regions.rs`) is O(n²) per pass and removes one component per pass.
Components are bounded by moving cells over `MIN_STREAM_CELLS`, which at 1080p went
from about 20 to about 102 — roughly 130× the work per retune. It runs twice a
second on integers and is not worth changing at `RETUNE = 500 ms` and
`MAX_STREAMS = 4`: a synthetic worst case of disjoint five-cell bars measures
0.2 ms for the 45 components a 1080p grid can hold and 4.6 ms for the 170 a 4K
grid can, in a release build. It is written down so it is not a surprise if either
moves.
