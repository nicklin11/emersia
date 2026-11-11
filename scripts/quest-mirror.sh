#!/usr/bin/env bash
# Mirror a host output to a USB-connected Quest, end to end, in one command.
#
#   scripts/quest-mirror.sh [--output DP-3] [--port 47800] [--fps 60]
#                           [--bitrate 30M] [--no-build] [--restart-daemon]
#
# Builds the host and the APK, installs the APK over adb, pairs the headset's
# identity with the daemon (reading only its *public* key), starts streaming
# and launches the client pointed at this machine's address on the headset's
# network. Safe to re-run: an already-paired headset is not paired again.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
pkg=org.emersia.client
output=""
port=47800
fps=60
bitrate=30M
build=1
restart=0

while (($#)); do
  case $1 in
    --output) output=$2; shift 2 ;;
    --port) port=$2; shift 2 ;;
    --fps) fps=$2; shift 2 ;;
    --bitrate) bitrate=$2; shift 2 ;;
    --no-build) build=0; shift ;;
    --restart-daemon) restart=1; shift ;;
    -h|--help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "quest-mirror: unknown argument $1" >&2; exit 2 ;;
  esac
done

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() { echo "quest-mirror: $*" >&2; exit 1; }

command -v adb >/dev/null || die "adb not found"
[[ $(adb get-state 2>/dev/null) == device ]] || die "no headset on adb (USB debugging allowed?)"

bin=$root/host/target/release
if ((build)); then
  say "building host"
  cargo build --release --quiet --manifest-path "$root/host/Cargo.toml"
  say "building client APK"
  "$root/client/android/build.sh" >/dev/null
fi
emersia=$bin/emersia
[[ -x $emersia ]] || die "no $emersia (drop --no-build)"

say "installing APK"
adb install -r "$root/client/android/build/emersia.apk" >/dev/null

daemon_log=${XDG_RUNTIME_DIR:-/tmp}/emersia-daemon.log
if ((restart)); then
  pkill -x emersia-daemon || true
  sleep 0.5
fi
if ! "$emersia" status >/dev/null 2>&1; then
  say "starting emersia-daemon (stream port $port, log $daemon_log)"
  setsid "$bin/emersia-daemon" serve --stream-port "$port" --bitrate "$bitrate" \
    >"$daemon_log" 2>&1 < /dev/null &
  for _ in $(seq 50); do "$emersia" status >/dev/null 2>&1 && break; sleep 0.1; done
  "$emersia" status >/dev/null 2>&1 || die "daemon did not come up; see $daemon_log"
fi

# The app writes files/device.pub ("<public key> <device id>") on first start.
say "reading headset identity"
adb shell am start -n "$pkg/.MainActivity" >/dev/null
ident=""
for _ in $(seq 50); do
  ident=$(adb shell run-as "$pkg" cat files/device.pub 2>/dev/null || true)
  [[ -n $ident ]] && break
  sleep 0.1
done
[[ -n $ident ]] || die "headset did not report an identity (adb logcat -s emersia)"
read -r device_pub device_id <<<"$ident"

if "$emersia" devices --json | python3 -c '
import json, sys
want = sys.argv[1]
devs = json.load(sys.stdin).get("devices", [])
sys.exit(0 if any(d["id"] == want and not d.get("revoked") for d in devs) else 1)
' "$device_id"; then
  say "headset $device_id already paired"
else
  say "pairing headset $device_id"
  code=$("$emersia" pair new --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["code"])')
  model=$(adb shell getprop ro.product.model | tr -d '\r' | tr ' ' '-')
  "$emersia" pair accept "$code" --name "${model:-quest}" --public-key "$device_pub" >/dev/null
fi

# This machine's address as seen from the headset's network.
quest_ip=$(adb shell ip -4 -o addr show wlan0 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | tr -d '\r')
[[ -n $quest_ip ]] || die "headset has no Wi-Fi address"
host_ip=$(ip -4 route get "$quest_ip" | grep -oP 'src \K\S+')
[[ -n $host_ip ]] || die "no route to headset at $quest_ip"
host_key=$("$emersia" status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["host_public_key"])')

if [[ $("$emersia" status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["streaming"])') != True ]]; then
  say "streaming ${output:-default output} at $fps fps"
  "$emersia" --fps "$fps" start $output >/dev/null
fi

# A host firewall is the usual reason the handshake gets no answer. ufw keeps
# its rules readable without root, so check and print the exact fix.
if systemctl is-active --quiet ufw 2>/dev/null && [[ -r /etc/ufw/user.rules ]] \
  && ! grep -qE "^### tuple ### allow (udp|any) $port " /etc/ufw/user.rules; then
  iface=$(ip -4 route get "$quest_ip" | grep -oP 'dev \K\S+')
  echo "quest-mirror: ufw is active and UDP $port is not allowed; the headset will get no answer." >&2
  echo "  sudo ufw allow in on $iface proto udp to any port $port comment 'Emersia stream'" >&2
fi

say "launching client → $host_ip:$port (headset at $quest_ip)"
adb shell am start -n "$pkg/.MainActivity" \
  --es server "$host_ip:$port" --es host_key "$host_key" >/dev/null
echo "Put the headset on. Status: adb logcat -s emersia · $emersia status"
