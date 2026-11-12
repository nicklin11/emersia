#!/usr/bin/env bash
# End-to-end QA of the portal capture backend — nothing appears on screen.
#
#   scripts/qa-portal-headless.sh [--frames N]
#
# Builds a private desktop: its own D-Bus session, PipeWire + WirePlumber,
# headless sway, and xdg-desktop-portal with the wlr backend configured to
# share HEADLESS-1 without a chooser dialog. Then `emersia-daemon serve
# --backend portal` streams through ScreenCast + PipeWire to a test receiver,
# and `emersia stop` must close the portal session (the desktop's sharing
# indicator). Needs: sway, pipewire, wireplumber, xdg-desktop-portal,
# xdg-desktop-portal-wlr, dbus, mpv (moving content).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
frames=30
while (($#)); do
  case $1 in
    --frames) frames=$2; shift 2 ;;
    -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    --inside) inside=1; shift ;;
    *) echo "qa-portal: unknown argument $1" >&2; exit 2 ;;
  esac
done

bin=${EMERSIA_BIN:-$root/host/target/release}
[[ -x $bin/emersia-daemon && -x $bin/emersia ]] || {
  echo "qa-portal: build first: cargo build --release --manifest-path host/Cargo.toml" >&2; exit 2; }
# Distros install the portal daemons under lib or libexec.
find_libexec() { for d in /usr/libexec /usr/lib; do [[ -x $d/$1 ]] && { echo "$d/$1"; return; }; done; }
xdp=$(find_libexec xdg-desktop-portal)
xdpw=$(find_libexec xdg-desktop-portal-wlr)
for need in sway pipewire wireplumber dbus-run-session mpv; do
  command -v "$need" >/dev/null || { echo "qa-portal: $need not installed" >&2; exit 2; }
done
[[ -n $xdp && -n $xdpw ]] || { echo "qa-portal: xdg-desktop-portal(-wlr) not installed" >&2; exit 2; }

# Re-exec inside a private D-Bus session.
if [[ -z ${inside:-} ]]; then
  exec dbus-run-session -- env -u WAYLAND_DISPLAY -u SWAYSOCK -u DISPLAY \
    "$0" --inside --frames "$frames"
fi

export XDG_RUNTIME_DIR
XDG_RUNTIME_DIR=$(mktemp -d /tmp/emp.XXXXXX)
chmod 700 "$XDG_RUNTIME_DIR"
work=$XDG_RUNTIME_DIR/work
mkdir -p "$work"
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  sleep 0.3
  for p in "${pids[@]}"; do kill -9 "$p" 2>/dev/null || true; done
  rm -rf "$XDG_RUNTIME_DIR"
}
trap cleanup EXIT
step() { printf '\033[1m==> %s\033[0m\n' "$*"; }
fail() { echo "qa-portal: FAIL: $*" >&2; for f in "$work"/*.log; do echo "--- $f" >&2; tail -15 "$f" >&2; done; exit 1; }

export XDG_CONFIG_HOME=$work/cfg XDG_CURRENT_DESKTOP=sway
mkdir -p "$XDG_CONFIG_HOME/xdg-desktop-portal-wlr" "$XDG_CONFIG_HOME/xdg-desktop-portal"
printf '[screencast]\nchooser_type=none\noutput_name=HEADLESS-1\n' \
  >"$XDG_CONFIG_HOME/xdg-desktop-portal-wlr/config"
printf '[preferred]\ndefault=wlr\norg.freedesktop.impl.portal.ScreenCast=wlr\n' \
  >"$XDG_CONFIG_HOME/xdg-desktop-portal/sway-portals.conf"

step "starting headless sway, PipeWire, WirePlumber and the portals"
printf 'output HEADLESS-1 mode 1280x720\n' >"$work/sway.conf"
WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
  sway -c "$work/sway.conf" >"$work/sway.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do [[ -S $XDG_RUNTIME_DIR/wayland-1 ]] && compgen -G "$XDG_RUNTIME_DIR/sway-ipc.*" >/dev/null && break; sleep 0.1; done
export WAYLAND_DISPLAY=wayland-1 SWAYSOCK
SWAYSOCK=$(compgen -G "$XDG_RUNTIME_DIR/sway-ipc.*" | head -1) || fail "sway did not start"
for _ in $(seq 50); do swaymsg -t get_version >/dev/null 2>&1 && break; sleep 0.1; done
pipewire >"$work/pipewire.log" 2>&1 & pids+=($!)
for _ in $(seq 50); do [[ -S $XDG_RUNTIME_DIR/pipewire-0 ]] && break; sleep 0.1; done
wireplumber >"$work/wireplumber.log" 2>&1 & pids+=($!)
"$xdpw" -l INFO >"$work/xdpw.log" 2>&1 & pids+=($!)
"$xdp" -r >"$work/xdp.log" 2>&1 & pids+=($!)
sleep 2

step "emersia-daemon serve --backend portal"
port=$((40000 + RANDOM % 20000))
"$bin/emersia-daemon" serve --backend portal --stream-port "$port" --socket "$work/ctl.sock" \
  --no-audio --no-notify --no-input >"$work/daemon.log" 2>&1 &
pids+=($!)
ctl() { "$bin/emersia" --socket "$work/ctl.sock" "$@"; }
for _ in $(seq 50); do ctl status >/dev/null 2>&1 && break; sleep 0.1; done
ctl status >/dev/null || fail "daemon did not start"
keys=$("$bin/emersia-daemon" keygen 2>/dev/null)
priv=$(awk '/private_key/{print $2}' <<<"$keys")
pub=$(awk '/public_key/{print $2}' <<<"$keys")
code=$(ctl pair new --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["code"])')
device=$(ctl pair accept "$code" --name qa --public-key "$pub" --json \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["device"])')
host_key=$(ctl status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["host_public_key"])')

# Keep the screen changing: the portal only sends frames on damage. An
# animated mpv window is real content; do not change the output background
# instead — sway applies `output … bg` as an output reconfiguration, the
# portal renegotiates, and xdg-desktop-portal-wlr 0.8 crashes on that with
# sway 1.12 ("session already has a frame object").
command -v mpv >/dev/null || fail "mpv is needed for moving screen content"
mpv --really-quiet --vo=wlshm --title=emersia-qa --loop \
  av://lavfi:testsrc=size=640x480:rate=30 >/dev/null 2>&1 &
pids+=($!)
sleep 1

step "streaming through ScreenCast + PipeWire"
ctl --fps 30 start >/dev/null
timeout 60 "$bin/emersia-daemon" receive --server "127.0.0.1:$port" --device-id "$device" \
  --host-key "$host_key" --private-key "$priv" --frames "$frames" --timeout 15 \
  | tee "$work/receive.log" || fail "receiver failed"
grep -q "^decoded:" "$work/receive.log" || fail "portal stream did not decode"
backend=$(ctl status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["backend"])')
encoder=$(ctl status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["encoder"])')
[[ $backend == portal ]] || fail "backend is $backend, expected portal"
[[ -s $work/cfg/emersia/portal-restore-token-0 ]] || echo "note: the portal returned no restore token"

step "stop must close the portal session (desktop sharing indicator)"
ctl stop >/dev/null
for _ in $(seq 30); do grep -q "session closed" "$work/xdpw.log" && break; sleep 0.1; done
grep -q "session closed" "$work/xdpw.log" || fail "portal session still open after stop"

echo "qa-portal: PASS — $frames frames via portal (sway $(swaymsg -t get_version | python3 -c 'import json,sys; print(json.load(sys.stdin)["human_readable"])'), encoder $encoder)"
