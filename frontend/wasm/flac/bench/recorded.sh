#!/usr/bin/env bash
# bench/recorded.sh NAME FILE [START]: a recording made a sample beside the generated ones. A minute of FILE, any sound
# ffmpeg reads, from START seconds in (60), becomes NAME-48000 and NAME-44100 in tmp/flac-bench: 16-bit stereo at each
# rate a session carries, and the frames the gateway's encoder makes of it. Name them in SAMPLES for bench/run.sh.
# What was measured with one is the scherzo of Beethoven's third symphony as the Musopen Symphony recorded it, in the
# public domain, from Wikimedia Commons:
#   https://commons.wikimedia.org/wiki/File:Beethoven_-_Symphony_No._3_in_E_flat_major,_Op._55_'Eroica'_-_III._Scherzo._Allegro_vivace_(Musopen_Symphony).flac
set -euo pipefail
cd "$(dirname "$0")/.."
[ $# -ge 2 ] || { echo "usage: bench/recorded.sh NAME FILE [START]" >&2; exit 1; }
dir=../../../tmp/flac-bench
mkdir -p $dir
cargo build --release --quiet --manifest-path bench/native/Cargo.toml
for rate in 48000 44100; do
  ffmpeg -hide_banner -loglevel error -y -ss "${3:-60}" -t 60 -i "$2" -ac 2 -ar $rate -f s16le "$dir/$1-$rate.pcm"
  bench/native/target/release/flac-bench make $dir "$1-$rate"
done
