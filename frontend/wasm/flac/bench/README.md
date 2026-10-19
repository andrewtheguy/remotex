# The FLAC decoder's benchmark

What a frame of lossless sound costs the page to decode, against libFLAC and
against another build of the module.

```sh
bench/run.sh                    # the module in pkg/
bench/run.sh BASE pkg libflac   # another build, this one, and libFLAC, in turn
```

A build is a directory holding `remotex_flac.js` and `remotex_flac_bg.wasm`:
copy `pkg/` aside before a change to have the one to compare with. `libflac`
is libFLAC decoding natively through `sound-flac`, as wlshare's and the
gateway's own readers would. Each decodes a sample forty times over under
`perf stat`, the builds in turn on one core, three rounds, and the medians are
printed per frame: the user-space instructions and cycles of the whole
process, which do not depend on the host's load. The module is driven as the
page drives it (`bench/decode.ts`): a frame through the binding, and each
channel copied out of the module's memory, under Bun. The first build named is
first checked, every sample of every frame, against the signal the frames were
made from.

## The samples

A minute each, 3,000 frames, made the first time into `tmp/flac-bench` at the
repository's root by `bench/native`, which encodes them with `sound-flac` as
the gateway encodes an RDP host's sound and wlshare its own: every frame a
stream of its own, at libFLAC's defaults. A name is a signal and a rate,
48 kHz in blocks of 960 as wlshare sends and 44.1 kHz in blocks of 882 as an
RDP host's is sent, in stereo.

Sound, unlike a picture, is as good generated as captured, so most are
generated, and are the same bytes on every machine:

| sample | what it is | a frame |
|---|---|---|
| `silence` | nothing, which a desktop plays most of the time | 20 bytes |
| `tone` | a 440 Hz sine, the same in both channels | 434 bytes |
| `music` | a chord of notes with overtones, struck twice a second and panned apart, over quiet coloured noise | 1.8 kB |
| `noise` | loud low-passed noise, different in each channel | 3.1 kB |
| `white` | full-scale white noise, which is stored as it is | 3.9 kB |

`bench/recorded.sh NAME FILE` makes a sample of a minute of any recording
ffmpeg reads. The one measured below, `symphony`, is the second minute of the
scherzo of Beethoven's third symphony as the Musopen Symphony recorded it, in
the public domain
([Wikimedia Commons](https://commons.wikimedia.org/wiki/File:Beethoven_-_Symphony_No._3_in_E_flat_major,_Op._55_%27Eroica%27_-_III._Scherzo._Allegro_vivace_(Musopen_Symphony).flac)):
a quiet passage, 1.4 kB a frame, whose Rice parameters are 2 to 4 where
`music`'s are 5 and 6 and `noise`'s 10 to 12.

## What was measured

Thousands of cycles a frame, on one core of a six-core x86 workstation
(i5-8500T). "claxon" is the module as it was, the `claxon` crate behind the
same binding, which copied a frame's samples out twice.

| sample | claxon | the module | libFLAC |
|---|---|---|---|
| `silence-48000` | 24.1 | 8.5 | 29.2 |
| `tone-48000` | 72.4 | 21.9 | 50.6 |
| `music-48000` | 146.6 | 47.3 | 78.0 |
| `music-44100` | 136.3 | 43.7 | 73.1 |
| `symphony-48000` | 146.0 | 44.9 | 75.7 |
| `symphony-44100` | 135.3 | 42.0 | 70.2 |
| `noise-48000` | 165.2 | 48.1 | 80.5 |
| `white-48000` | 91.4 | 39.3 | 110.6 |

A frame is 20 ms of sound, so the module takes a thousandth of a core to
play music. libFLAC's column carries what `sound-flac` does around it, a
stream opened and closed for every frame, which is the 29 K of its silence.
The claxon column was measured on another day than the other two, which were
measured together.

## Where a frame's time goes

In the Rice codes of the residual, two fifths of a frame of music, and the
prediction, over a quarter; the checksum is a twelfth, and the floats and the
binding the rest. Each of the two is a loop in which a sample waits on the one
before: a code on where the code before ended, a count of zeros and a rotation
away, and a predicted sample on the sample before it, a product and a sum
away. Both are near what those waits cost, so fewer instructions in either no
longer make a frame cheaper (below). What each of the decoder's choices is
worth on `music` at 48 kHz, in thousands of cycles a frame, each row with the
rows above it:

| the decoder | a frame |
|---|---|
| the `claxon` crate | 147 |
| written here, with the window of bits a field of the reader | 109 |
| the window in locals through a partition, and the prediction reading the block and not a copy of its last samples shifted along each time | 82 |
| the samples left in the module's memory, not copied out of it twice | 78 |
| SIMD, for the floats and the channels | 76 |
| the frame laid out once as words, so no load turns bytes round | 69 |
| as many codes to a window as the parameter says it mostly holds, two to eight | 58 |
| each code rotated round the window, so the run, the one and the low bits are one rotation and a mask, and whether the window held its codes whole asked once for them all | 53 |
| the prediction from all but the sample before taken a sample ahead, so a sample waits on one product and one sum | 50 |
| the reader's place in a local through a partition, where with the codes rotated it was written back after every window | 49 |
| the next window spliced from what is left of this one and the bits after it, loaded while its codes are read, so it does not wait on a load | 47 |

Two choices do nothing for `music` and are kept for other sound. A fixed
predictor's coefficients, known in the code, become shifts and sums: `tone`,
which is all fourth-order fixed, 26.8 K to 25.0. And wasm-opt is told not to
fold a function with one caller into it (`--one-caller-inline-max-function-size=0`
in `Cargo.toml`): folded, a subframe's decoding shares one function with the
whole frame's, and V8 keeps fewer of its values in registers. `noise` 49.5 K
to 48.0, `tone` 23.7 to 22.0.

## Tried and no faster

- **Four bytes put under the window without a branch** whenever it is down to
  32 bits, loaded a code ahead of being wanted, so that no code waits on a
  load whose place the code before decided: 83.9 K cycles against the 82.6 of
  a window filled once for two codes. The codes do not wait on the load.
- **One shift past a code**, by its whole length, where there were three: no
  fewer cycles on its own with two codes to a window, 69.1 K against 69.6. It
  is kept, as the simpler.
- **The run's place above the low bits as a product**, by two to the
  parameter, where it is a shift: 53.6 K against 53.1. On x86 V8 shifts and
  rotates by an amount in one register, which a product would leave to the
  rotation alone.
- **A number of codes to a window known in the code**, a function for each
  from two to eight, so a window's codes are laid out whole with no count:
  7 K fewer instructions a frame and 0.7 K more cycles, and with the window
  spliced 9 K fewer instructions and as many cycles. The codes wait on each
  other, not on the instructions.
- **The frame laid out last byte first**, so that one load at any byte is the
  window, where it is two words shifted together: 50.1 K against 50.3 on
  `music`, and `silence` 9.5 K against 8.1. Turning the frame round a byte at
  a time costs what the load saves.
- **The sum with the sample before added last**, as written in the loop that
  sums it: 57.3 K against 57.9. LLVM puts it back where it was.

## Profiling

Under Node, where V8 names a module's functions if the build keeps them:

```sh
cargo build --release --target wasm32-unknown-unknown
wasm-bindgen target/wasm32-unknown-unknown/release/remotex_flac.wasm --out-dir DIR --target web --keep-debug
FLAC_WASM_DIR=DIR perf record -- node --perf-basic-prof --no-liftoff bench/decode.ts FILE.frames 60
perf report --no-children --sort symbol
```

`wasm-bindgen` is the one `wasm-pack` fetched for the crate's version, under `~/.cache/.wasm-pack`,
and `wasm-objdump -d` of the module it writes shows what a loop was compiled
to. That module has not been through wasm-opt, which folds functions into one
another: `wasm-opt -g` with the flags in `Cargo.toml` gives the module that is
shipped, its names kept. `node --perf-prof` also writes V8's machine code for
it, as `jit-PID.dump`, for `perf inject --jit` and `perf annotate`: what V8
keeps in registers is where the last cycles went.
