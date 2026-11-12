#!/usr/bin/env bash
# Build and install the Emersia host for the current user.
#
#   scripts/install.sh [--no-enable]
#
# Installs emersia-daemon and emersia to ~/.local/bin, the systemd user unit to
# ~/.config/systemd/user, and enables + starts it (unless --no-enable). Prints
# what still needs root: the `input` group for headset input, and a firewall
# rule for the stream port when a firewall is active.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
enable=1
[[ ${1:-} == --no-enable ]] && enable=0
port=47800
bin_dir=$HOME/.local/bin
unit_dir=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }

say "building (release)"
cargo build --release --quiet --manifest-path "$root/host/Cargo.toml"

say "installing binaries to $bin_dir"
install -Dm755 "$root/host/target/release/emersia-daemon" "$bin_dir/emersia-daemon"
install -Dm755 "$root/host/target/release/emersia" "$bin_dir/emersia"

say "installing systemd user unit to $unit_dir"
install -Dm644 "$root/packaging/systemd/emersia-daemon.service" "$unit_dir/emersia-daemon.service"
systemctl --user daemon-reload

if ((enable)); then
  if ! systemctl --user show-environment | grep -q '^WAYLAND_DISPLAY='; then
    echo "note: the user manager has no WAYLAND_DISPLAY; under sway/hyprland add" >&2
    echo "  exec systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP" >&2
    echo "to your compositor config, then: systemctl --user enable --now emersia-daemon" >&2
  else
    say "enabling and (re)starting emersia-daemon"
    systemctl --user enable emersia-daemon.service >/dev/null
    systemctl --user restart emersia-daemon.service
    sleep 1
    systemctl --user --no-pager --lines=0 status emersia-daemon.service | head -3 || true
  fi
fi

# Things that need root are printed, never run.
if ! id -nG | tr ' ' '\n' | grep -qx input; then
  echo
  echo "Headset input (pointer/keyboard) needs /dev/uinput:"
  echo "  sudo usermod -aG input $USER   # then log out and back in"
fi
if systemctl is-active --quiet ufw 2>/dev/null && [[ -r /etc/ufw/user.rules ]] \
  && ! grep -qE "^### tuple ### allow (udp|any) $port " /etc/ufw/user.rules; then
  echo
  echo "ufw is active; allow the stream port so the headset can connect:"
  echo "  sudo ufw allow proto udp to any port $port comment 'Emersia stream'"
elif systemctl is-active --quiet firewalld 2>/dev/null; then
  echo
  echo "firewalld is active; allow the stream port so the headset can connect:"
  echo "  sudo firewall-cmd --permanent --add-port=$port/udp && sudo firewall-cmd --reload"
fi
echo
echo "Done. Pair a headset: scripts/quest-mirror.sh (USB) — or see README."
