#!/usr/bin/env bash
# Capture a High Performance Mac's media stream as the gateway receives it.
#
# Builds the gateway, serves CONFIG with REMOTEX_HP_DUMP set,
# drives one session through tests/ws_probe.py with its audio socket open, stops the
# gateway and summarises the capture:
#
#   OUT/video.h265   every HEVC access unit, Annex B (ffprobe/ffplay read it as is)
#   OUT/audio.eld    every AAC-ELD unit behind its length as a big-endian u32
#   OUT/gateway.log  the gateway's debug log for the session
#   OUT/index.html   tests/hp_decode/, which tries the browser's WebCodecs on both
#
# Serve OUT to try them: `cd OUT && uv run python -m http.server 8000`, then open
# http://localhost:8000/ (localhost is a secure context, which WebCodecs needs).
#
# With --strips the session is started as the page's software decoder takes it, passed
# and in four strips where the display's height allows, each picture of video.h265 one
# strip: CONFIG must name the decoder ([hevc_wasm]). tests/hp_strips_video.py makes a
# video of the display from such a capture, and tests/hp_motion/ holds scripts to run on
# the Mac for something to capture. With --whole it is started as a browser's own decoder
# takes it, passed and each picture the whole display. With neither the gateway decodes
# the picture itself, from strips.
#
# The probe asks for no resize: the stream is offered only once the display has
# settled, and a resize in flight on a slow Mac can outlast the session. Play
# something on the Mac first if the capture should carry motion and sound.
#
#   REMOTEX_PROBE_PASSWORD=... tests/hp_capture.sh \
#       [--config tmp/test_uat_hp.toml] [--port 52888] [--target macvmhighperf] \
#       [--user admin] [--seconds 45] [--display 1440x900@200] [--out tmp/hp-capture] \
#       [--strips | --whole]
set -euo pipefail

cd "$(dirname "$0")/.."

config=tmp/test_uat_hp.toml
port=52888
target=macvmhighperf
user=admin
seconds=45
display=1440x900@200
out=tmp/hp-capture
passed=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --config) config=$2 ;;
    --port) port=$2 ;;
    --target) target=$2 ;;
    --user) user=$2 ;;
    --seconds) seconds=$2 ;;
    --display) display=$2 ;;
    --out) out=$2 ;;
    --strips) passed=(--passthrough --software); shift; continue ;;
    --whole) passed=(--passthrough --apple-media); shift; continue ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift 2
done

if [[ -z "${REMOTEX_PROBE_PASSWORD:-}" ]]; then
  echo "set REMOTEX_PROBE_PASSWORD to the gateway's password for $user" >&2
  exit 2
fi
if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "something already listens on $port; stop it or pass --port" >&2
  exit 1
fi

cargo build --profile qa

mkdir -p "$out"
rm -f "$out/video.h265" "$out/audio.eld"
cp tests/hp_decode/index.html tests/hp_decode/probe.js "$out/"
REMOTEX_HP_DUMP="$out" RUST_LOG=info,remotex=debug \
  ./target/qa/remotex serve --config "$config" --listen "127.0.0.1:$port" >"$out/gateway.log" 2>&1 &
gateway=$!
trap 'kill "$gateway" 2>/dev/null || true; wait "$gateway" 2>/dev/null || true' EXIT

for _ in $(seq 60); do
  grep -q "listening on" "$out/gateway.log" && break
  kill -0 "$gateway" 2>/dev/null || { cat "$out/gateway.log" >&2; exit 1; }
  sleep 0.5
done

uv run tests/ws_probe.py --port "$port" --target "$target" --user "$user" \
  --display "$display" --audio --seconds "$seconds" ${passed[@]+"${passed[@]}"}

kill "$gateway"
wait "$gateway" 2>/dev/null || true
trap - EXIT

echo
grep -E "dumping the media stream|screen video is flowing|sound is flowing|HEVC is decoded by|stopped dumping|not dumping" \
  "$out/gateway.log" | sed 's/^/  /' || true
if [[ ! -s "$out/video.h265" ]]; then
  echo "no HEVC was captured; the stream never started — see $out/gateway.log" >&2
  exit 1
fi
ls -l "$out/video.h265"
if [[ -s "$out/audio.eld" ]]; then
  ls -l "$out/audio.eld"
else
  echo "no AAC-ELD was captured; the sound leg never ran — see $out/gateway.log" >&2
fi
if command -v ffprobe >/dev/null; then
  ffprobe -v error -count_frames \
    -show_entries stream=codec_name,profile,width,height,pix_fmt,nb_read_frames \
    -of default=noprint_wrappers=1 "$out/video.h265"
fi
# 4-byte length + unit, 100 units a second.
uv run python - "$out/audio.eld" <<'EOF'
import os, struct, sys
if not os.path.exists(sys.argv[1]):
    sys.exit()
data = open(sys.argv[1], "rb").read()
at = units = 0
while at + 4 <= len(data):
    (n,) = struct.unpack_from(">I", data, at)
    at += 4 + n
    units += 1
print(f"audio.eld: {units} AAC-ELD units, {units / 100:.1f} s")
EOF
echo
echo "decode in the browser: cd $out && uv run python -m http.server 8000, then http://localhost:8000/"
