#!/usr/bin/env bash
# bench/run.sh [BUILD...]: the routine benchmark, a minute or two. Each BUILD is a directory holding a build of the
# module (remotex_flac.js, remotex_flac_bg.wasm), pkg when none is named, or the word libflac for libFLAC decoding
# natively through sound-flac. They decode the samples alternately, round by round on the same core, and the medians
# over the rounds are printed per frame: the user-space instructions and cycles of the whole process, which do not
# depend on the host's load, and the module's mean microseconds by its own clock.
#   ROUNDS=3 PASSES=40 SAMPLES="name ..." CPU=2
# The samples are generated, a minute each, into tmp/flac-bench at the repository's root the first time, and the first
# module named is first checked against every sample's signal.
set -euo pipefail
cd "$(dirname "$0")/.."
[ $# -gt 0 ] || set -- pkg
rounds=${ROUNDS:-3} passes=${PASSES:-40} cpu=${CPU:-2}
samples=${SAMPLES:-silence-48000 tone-48000 music-48000 music-44100 noise-48000 white-48000}
dir=../../../tmp/flac-bench
native=bench/native/target/release/flac-bench
cargo build --release --quiet --manifest-path bench/native/Cargo.toml
mkdir -p $dir
for s in $samples; do
  [ -e "$dir/$s.frames" ] || $native make $dir "$s"
done

for s in $samples; do
  [ "$1" = libflac ] && break
  FLAC_WASM_DIR=$1 CHECK=1 bun bench/decode.ts "$dir/$s.frames" 1 >/dev/null || { echo "$1 does not decode $s" >&2; exit 1; }
done

log=$(mktemp); trap 'rm -f "$log"' EXIT
for s in $samples; do
  : >"$log"
  for ((r = 0; r < rounds; r++)); do
    for b in "$@"; do
      if [ "$b" = libflac ]; then
        out=$(taskset -c "$cpu" perf stat -e instructions:u,cycles:u -x, $native decode "$dir/$s.frames" "$passes" 2>&1); mean=0
      else
        out=$(FLAC_WASM_DIR=$b taskset -c "$cpu" perf stat -e instructions:u,cycles:u -x, bun bench/decode.ts "$dir/$s.frames" "$passes" 2>&1)
        mean=$(grep -o 'mean [0-9.]*' <<<"$out" | cut -d' ' -f2)
      fi
      n=$(grep -o '^[0-9]* frames' <<<"$out" | cut -d' ' -f1)
      i=$(grep instructions <<<"$out" | cut -d, -f1); c=$(grep cycles <<<"$out" | cut -d, -f1)
      awk -v b="$b" -v n="$n" -v i="$i" -v c="$c" -v a="$mean" 'BEGIN {print b, i / n / 1e3, c / n / 1e3, a}' >>"$log"
    done
  done
  echo "-- $s, $n frames, cpu $cpu, medians of $rounds rounds, load $(cut -d' ' -f1-3 /proc/loadavg)"
  for b in "$@"; do
    med() { awk -v b="$b" '$1 == b' "$log" | cut -d' ' -f"$1" | sort -n | awk '{v[NR] = $1} END {print v[int((NR + 1) / 2)]}'; }
    printf '%-32s %8.1f K instr %8.1f K cycles   mean %6.2f us\n' "$b" "$(med 2)" "$(med 3)" "$(med 4)"
  done
done
