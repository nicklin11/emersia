#!/usr/bin/env bash
# End-to-end QA against a private, headless sway — nothing appears on screen.
#
#   scripts/qa-sway-headless.sh [--encoder auto|sw|vaapi…] [--frames N]
#
# Starts sway with the headless backend on its own Wayland socket, creates a
# second output at runtime (`swaymsg create_output`, sway's virtual outputs),
# and runs the real pipeline against it: one-shot capture, then
# `emersia-daemon serve` → pair a throwaway device → stream the virtual output
# → `emersia-daemon receive` decodes it with ffmpeg. Exits non-zero on any
# failure. Used by CI (software encoder) and for local sway QA.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
encoder=auto
frames=20
while (($#)); do
  case $1 in
    --encoder) encoder=$2; shift 2 ;;
    --frames) frames=$2; shift 2 ;;
    -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "qa-sway: unknown argument $1" >&2; exit 2 ;;
  esac
done

command -v sway >/dev/null || { echo "qa-sway: sway not installed" >&2; exit 2; }
bin=${EMERSIA_BIN:-$root/host/target/release}
[[ -x $bin/emersia-daemon && -x $bin/emersia ]] || {
  echo "qa-sway: build first: cargo build --release --manifest-path host/Cargo.toml" >&2; exit 2; }

# Unix socket paths are limited to 108 bytes, so keep the runtime dir short.
rt=$(mktemp -d /tmp/emqa.XXXXXX)
chmod 700 "$rt"
work=$(mktemp -d)
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "$rt" "$work"
}
trap cleanup EXIT

step() { printf '\033[1m==> %s\033[0m\n' "$*"; }
fail() { echo "qa-sway: FAIL: $*" >&2; for f in "$work"/*.log; do echo "--- $f" >&2; tail -20 "$f" >&2; done; exit 1; }

printf 'output HEADLESS-1 mode 1280x720\n' >"$work/sway.conf"
step "starting headless sway"
env -i HOME="$HOME" PATH="$PATH" XDG_RUNTIME_DIR="$rt" \
  WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
  sway -c "$work/sway.conf" >"$work/sway.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do [[ -S $rt/wayland-1 ]] && compgen -G "$rt/sway-ipc.*" >/dev/null && break; sleep 0.1; done
[[ -S $rt/wayland-1 ]] || fail "sway did not start"
export SWAYSOCK
SWAYSOCK=$(compgen -G "$rt/sway-ipc.*" | head -1)
# The IPC socket exists before sway answers on it; a request sent too early
# fails with "Unable to receive IPC response" (seen in CI).
for _ in $(seq 50); do swaymsg -t get_version >/dev/null 2>&1 && break; sleep 0.1; done
swaymsg -t get_version >/dev/null || fail "sway IPC did not come up"

step "creating a virtual output (swaymsg create_output)"
swaymsg create_output >/dev/null
swaymsg output HEADLESS-2 mode 1920x1080 pos 1280 0 >/dev/null
swaymsg -t get_outputs | grep -q '"HEADLESS-2"' || fail "virtual output was not created"

run() { env XDG_RUNTIME_DIR="$rt" WAYLAND_DISPLAY=wayland-1 XDG_CONFIG_HOME="$work/config" "$@"; }

step "one-shot capture of the virtual output"
run timeout 30 "$bin/emersia-daemon" capture --output HEADLESS-2 --out "$work/frame.png" | tee "$work/capture.log"
grep -q "captured 1920x1080" "$work/capture.log" || fail "capture did not return 1920x1080"

step "streaming the virtual output to a test receiver"
port=$((40000 + RANDOM % 20000))
# Started directly (not through `run`): `$!` must be the daemon itself, or
# cleanup kills a wrapper subshell and leaves the daemon running.
env XDG_RUNTIME_DIR="$rt" WAYLAND_DISPLAY=wayland-1 XDG_CONFIG_HOME="$work/config" \
  "$bin/emersia-daemon" serve --stream-port "$port" --socket "$rt/ctl.sock" \
  --encoder "$encoder" >"$work/daemon.log" 2>&1 &
pids+=($!)
ctl() { "$bin/emersia" --socket "$rt/ctl.sock" "$@"; }
for _ in $(seq 50); do ctl status >/dev/null 2>&1 && break; sleep 0.1; done
ctl status >/dev/null || fail "daemon did not start"

keys=$("$bin/emersia-daemon" keygen)
priv=$(awk '/private_key/{print $2}' <<<"$keys")
pub=$(awk '/public_key/{print $2}' <<<"$keys")
code=$(ctl pair new --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["code"])')
device=$(ctl pair accept "$code" --name qa --public-key "$pub" --json \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["device"])')
host_key=$(ctl status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["host_public_key"])')
step "creating a virtual output with 'emersia virtual add' while the daemon runs"
virt=$("$bin/emersia" virtual add 1280x720) || fail "emersia virtual add failed"
echo "created $virt"
# The daemon re-reads outputs once a second (hot plug).
for _ in $(seq 50); do
  ctl screens --json | grep -q "\"$virt\"" && break
  sleep 0.1
done
ctl screens --json | grep -q "\"$virt\"" || fail "daemon did not pick up $virt"

step "streaming HEADLESS-2 and $virt at once (two streams)"
ctl --fps 30 start HEADLESS-2 "$virt" >/dev/null

# Capture is damage-driven: keep the output changing while we receive.
(for i in $(seq 400); do
   c=$(printf '%02x%02x%02x' $((i * 7 % 256)) 80 $((255 - i * 7 % 256)))
   swaymsg "output HEADLESS-2 bg #$c solid_color" >/dev/null 2>&1 || exit 0
   swaymsg "output $virt bg #$c solid_color" >/dev/null 2>&1 || exit 0
   sleep 0.05
 done) &
pids+=($!)

for stream in 0 1; do
  timeout 90 "$bin/emersia-daemon" receive --server "127.0.0.1:$port" --device-id "$device" \
    --host-key "$host_key" --private-key "$priv" --frames "$frames" --timeout 10 \
    --stream "$stream" | tee "$work/receive-$stream.log" || fail "receiver failed (stream $stream)"
  grep -q "^decoded:" "$work/receive-$stream.log" || fail "stream $stream did not decode"
done

step "removing $virt"
"$bin/emersia" virtual remove "$virt" || fail "emersia virtual remove failed"
! "$bin/emersia" virtual list | grep -qx "$virt" || fail "$virt still listed"
backend=$(grep -oP 'capture backend: \K.*' "$work/daemon.log")
encoder_used=$(ctl status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["encoder"])')
echo "qa-sway: PASS (2 streams incl. virtual output $virt) — $(swaymsg -t get_version | python3 -c 'import json,sys; print(json.load(sys.stdin)["human_readable"])'), backend $backend, encoder $encoder_used"
