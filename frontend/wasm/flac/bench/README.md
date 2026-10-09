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
| `silence-48000` | 24.1 | 8.8 | 26.7 |
| `tone-48000` | 72.4 | 30.1 | 48.9 |
| `music-48000` | 146.6 | 57.6 | 75.1 |
| `music-44100` | 136.3 | 53.2 | 70.6 |
| `symphony-48000` | 146.0 | 55.3 | 72.0 |
| `symphony-44100` | 135.3 | 51.3 | 67.2 |
| `noise-48000` | 165.2 | 57.1 | 78.2 |
| `white-48000` | 91.4 | 40.6 | 98.4 |

A frame is 20 ms of sound, so the module takes a thousandth of a core to
play music. libFLAC's column carries what `sound-flac` does around it, a
stream opened and closed for every frame, which is the 27 K of its silence.

## Where a frame's time goes

In the Rice codes of the residual, half of a frame of music, and the
prediction, a fifth; the checksum is a twentieth, and the floats and the
binding the rest. What each of the decoder's choices is worth on `music` at
48 kHz, in thousands of cycles a frame, each row with the rows above it:

| the decoder | a frame |
|---|---|
| the `claxon` crate | 147 |
| written here, with the window of bits a field of the reader | 109 |
| the window in locals through a partition, and the prediction reading the block and not a copy of its last samples shifted along each time | 82 |
| the samples left in the module's memory, not copied out of it twice | 78 |
| SIMD, for the floats and the channels | 76 |
| the frame laid out once as words, so no load turns bytes round | 69 |
| as many codes to a window as the parameter says it mostly holds, two to eight | 58 |

## Tried and no faster

- **Four bytes put under the window without a branch** whenever it is down to
  32 bits, loaded a code ahead of being wanted, so that no code waits on a
  load whose place the code before decided: 83.9 K cycles against the 82.6 of
  a window filled once for two codes. The codes do not wait on the load.
- **One shift past a code**, by its whole length, where there were three: no
  fewer cycles on its own with two codes to a window, 69.1 K against 69.6. It
  is kept, as the simpler.
- **The reader's place in a local through a partition**, written back at its
  end: 58.7 K against 58.6. The compiler had it there already.

## Profiling

Under Node, where V8 names a module's functions if the build keeps them:

```sh
cargo build --release --target wasm32-unknown-unknown
wasm-bindgen target/wasm32-unknown-unknown/release/remotex_flac.wasm --out-dir DIR --target web --keep-debug
FLAC_WASM_DIR=DIR perf record -- node --perf-basic-prof --no-liftoff bench/decode.ts FILE.frames 60
perf report --no-children --sort symbol
```

`wasm-bindgen` is the one `wasm-pack` fetched, under `~/.cache/.wasm-pack`,
and `wasm-objdump -d` of the module it writes shows what a loop was compiled
to.
