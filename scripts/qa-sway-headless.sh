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
  # A process blocked on a hung compositor never finishes SIGTERM handling;
  # give a grace period, then SIGKILL, so cleanup itself cannot hang.
  for _ in $(seq 30); do
    alive=0
    for p in "${pids[@]}"; do kill -0 "$p" 2>/dev/null && alive=1; done
    ((alive)) || break
    sleep 0.1
  done
  for p in "${pids[@]}"; do kill -9 "$p" 2>/dev/null || true; done
  pkill -9 -P $$ 2>/dev/null || true
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
# Three small extra outputs: with the virtual one created later, six streams
# run at once (more than the old 4-stream cap; #142).
small=(HEADLESS-3 HEADLESS-4 HEADLESS-5)
for i in 0 1 2; do
  swaymsg create_output >/dev/null
  swaymsg output "${small[$i]}" mode 640x360 pos $((i * 640)) 1080 >/dev/null
done
for o in "${small[@]}"; do swaymsg -t get_outputs | grep -q "\"$o\"" || fail "$o was not created"; done

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
  --encoder "$encoder" --no-audio >"$work/daemon.log" 2>&1 &
daemon_pid=$!
pids+=($daemon_pid)
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

targets=(HEADLESS-2 "$virt" HEADLESS-1 "${small[@]}")
step "streaming ${#targets[@]} outputs at once: ${targets[*]}"
ctl --fps 30 start "${targets[@]}" >/dev/null

# Capture is damage-driven: keep the output changing while we receive.
# The virtual output gets no `bg` (a swaybg left on an output that is later
# unplugged spins and starves sway 1.11's IPC); its damage is the cursor,
# which captures now composite, moving back and forth across it.
read -r vx vy vw vh < <(swaymsg -r -t get_outputs | python3 -c '
import json, sys
o = [o for o in json.load(sys.stdin) if o["name"] == sys.argv[1]][0]["rect"]
print(o["x"], o["y"], o["width"], o["height"])' "$virt")
(for i in $(seq 400); do
   c=$(printf '%02x%02x%02x' $((i * 7 % 256)) 80 $((255 - i * 7 % 256)))
   for o in HEADLESS-2 HEADLESS-1 "${small[@]}"; do
     swaymsg "output $o bg #$c solid_color" >/dev/null 2>&1 || exit 0
   done
   swaymsg "seat seat0 cursor set $((vx + 50 + i * 37 % (vw - 100))) $((vy + vh / 2))" >/dev/null 2>&1 || exit 0
   sleep 0.05
 done) &
damage_pid=$!
pids+=($damage_pid)

for stream in $(seq 0 $((${#targets[@]} - 1))); do
  timeout 90 "$bin/emersia-daemon" receive --server "127.0.0.1:$port" --device-id "$device" \
    --host-key "$host_key" --private-key "$priv" --frames "$frames" --timeout 10 \
    --stream "$stream" | tee "$work/receive-$stream.log" || fail "receiver failed (stream $stream)"
  grep -q "^decoded:" "$work/receive-$stream.log" || fail "stream $stream did not decode"
done

encoder_used=$(ctl status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["encoder"])')
backend=$(grep -oP 'capture backend: \K.*' "$work/daemon.log")

step "removing $virt"
# Quiesce first: stop the background-colour loop (each `bg` spawns a
# swaybg client; one left on an unplugged output spins and starves sway's
# IPC — seen on sway 1.11) and stop capturing the output.
kill "$damage_pid" 2>/dev/null || true
wait "$damage_pid" 2>/dev/null || true
ctl stop >/dev/null
# Then let the daemon go entirely: on sway 1.11 an unplug did not return while
# a client still had the output bound, so check removal with no client.
kill "$daemon_pid" 2>/dev/null || true
for _ in $(seq 30); do kill -0 "$daemon_pid" 2>/dev/null || break; sleep 0.1; done
sleep 0.5
"$bin/emersia" virtual remove "$virt" || fail "emersia virtual remove failed"
! "$bin/emersia" virtual list | grep -qx "$virt" || fail "$virt still listed"
echo "qa-sway: PASS (${#targets[@]} streams incl. virtual output $virt) — $(swaymsg -t get_version | python3 -c 'import json,sys; print(json.load(sys.stdin)["human_readable"])'), backend $backend, encoder $encoder_used"
