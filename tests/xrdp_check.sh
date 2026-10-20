#!/usr/bin/env bash
# The gateway against xrdp, with egfx = false: the desktop arrives as tiles,
# typing reaches the session, and the clipboard goes both ways. Nothing else is
# asked for; xrdp is supported as a plain VNC server is.
#
#   tests/xrdp_check.sh                          # xrdp's security_layer=negotiate
#   XRDP_SECURITY_LAYER=tls tests/xrdp_check.sh
#
# Needs podman or docker, cargo and uv. It builds tests/xrdp-dummy, starts the
# gateway on a config of its own, drives a session with tests/ws_probe.py, and
# prints PASS or FAIL for each check. The config and logs are left in tmp/xrdp/.
set -euo pipefail

cd "$(dirname "$0")/.."
out=tmp/xrdp
mkdir -p "$out"
runtime=${CONTAINER_RUNTIME:-$(command -v podman >/dev/null && echo podman || echo docker)}
image=remotex-xrdp-dummy
container=remotex-xrdp-dummy
rdp_port=${RDP_PORT:-53389}
gw_port=${GW_PORT:-52895}
gw_password=admin

cleanup() {
    [[ -n ${probe_pid:-} ]] && kill "$probe_pid" 2>/dev/null || true
    [[ -n ${gw_pid:-} ]] && kill "$gw_pid" 2>/dev/null || true
    "$runtime" rm -f "$container" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Wait for something to listen on a local port.
listening() {
    for _ in $(seq 1 30); do
        nc -z 127.0.0.1 "$1" 2>/dev/null && return 0
        sleep 1
    done
    echo "nothing listens on port $1" >&2
    return 1
}

"$runtime" build -q -t "$image" -f tests/xrdp-dummy/Containerfile tests/xrdp-dummy >/dev/null
"$runtime" rm -f "$container" >/dev/null 2>&1 || true
"$runtime" run -d --name "$container" -p "127.0.0.1:$rdp_port:3389" \
    -e "XRDP_SECURITY_LAYER=${XRDP_SECURITY_LAYER:-negotiate}" "$image" >/dev/null
listening "$rdp_port"

hash=$(uv run -q --with bcrypt python -c \
    "import bcrypt,sys; print(bcrypt.hashpw(sys.argv[1].encode(), bcrypt.gensalt(12)).decode())" \
    "$gw_password")
cat >"$out/test_xrdp.toml" <<EOF
[server]
listen = "127.0.0.1:$gw_port"
# admin password = $gw_password
site_passwd = "admin:$hash"

[[targets]]
name = "xrdp"
protocol = "rdp"
host = "127.0.0.1"
port = $rdp_port
username = "rdpuser"
password = "secret42"
egfx = false
EOF

cargo build -q
RUST_LOG=${RUST_LOG:-remotex=debug} target/debug/remotex serve --config "$out/test_xrdp.toml" \
    >"$out/gateway.log" 2>&1 &
gw_pid=$!
listening "$gw_port"

# The probe pushes its clipboard text and, after --key-delay, types
# `touch /tmp/typed` and Enter into the xterm. Meanwhile the remote side is
# checked from inside the container.
typed=(KeyT KeyO KeyU KeyC KeyH Space Slash KeyT KeyM KeyP Slash KeyT KeyY KeyP KeyE KeyD Enter)
chords=()
for code in "${typed[@]}"; do chords+=(--chord "$code"); done
REMOTEX_PROBE_PASSWORD=$gw_password uv run tests/ws_probe.py \
    --port "$gw_port" --target xrdp --user admin --seconds 20 \
    --clipboard "hello from remotex" \
    --key-delay 4 --chord-gap 0.1 "${chords[@]}" >"$out/probe.out" 2>&1 &
probe_pid=$!

in_session() {
    "$runtime" exec -u rdpuser -e DISPLAY=:10 -e XAUTHORITY=/home/rdpuser/.Xauthority \
        "$container" "$@"
}
sleep 10
pasted=$(in_session timeout 5 xclip -o -selection clipboard 2>&1 || true)
in_session sh -c 'printf "hello from xrdp" | timeout 5 xclip -i -selection clipboard -loops 1' &
probe_status=0
wait "$probe_pid" || probe_status=$?
probe_pid=
cat "$out/probe.out"

status=0
pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; status=1; }
[[ $probe_status -eq 0 ]] && pass "session ran without an error" || fail "probe exited $probe_status"
grep -q tilesStart "$out/probe.out" && grep -qE '^ *[1-9][0-9]* binary frames' "$out/probe.out" \
    && pass "desktop picture arrived as tiles" || fail "no picture"
"$runtime" exec "$container" test -f /tmp/typed \
    && pass "keyboard: the typed command ran" || fail "keyboard: /tmp/typed is missing"
[[ $pasted == "hello from remotex" ]] \
    && pass "clipboard to remote: xclip read the pushed text" || fail "clipboard to remote: got '$pasted'"
grep -q "clipboard (pushed).*hello from xrdp" "$out/probe.out" \
    && pass "clipboard from remote: the probe got the copy" || fail "clipboard from remote: nothing arrived"

echo "--- gateway log: $out/gateway.log"
grep -E "WARN|ERROR" "$out/gateway.log" | tail -10 || true
exit "$status"
