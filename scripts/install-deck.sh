#!/usr/bin/env bash
# Deck-side installer. Idempotent.
set -euo pipefail

if [[ "$EUID" -ne 0 ]]; then
    echo "Run as root: sudo $0" >&2
    exit 1
fi

if ! command -v usbip >/dev/null; then
    if command -v steamos-readonly >/dev/null; then
        echo ">> SteamOS detected. Disabling readonly + installing usbip..."
        steamos-readonly disable
        if [[ ! -f /etc/pacman.d/gnupg/trustdb.gpg ]]; then
            pacman-key --init
            pacman-key --populate
        fi
        pacman -S --noconfirm usbip
        steamos-readonly enable
    else
        echo "Install the 'usbip' package via your distro's package manager, then re-run." >&2
        exit 1
    fi
fi

echo ">> Loading kernel modules..."
modprobe usbip-core
modprobe usbip-host
modprobe vhci-hcd
cat >/etc/modules-load.d/usbip.conf <<'EOF'
usbip-core
usbip-host
vhci-hcd
EOF

echo ">> Enabling usbipd.service..."
systemctl enable --now usbipd.service

BIN_SRC="$(dirname "$0")/../target/release/server-deck"
if [[ ! -f "$BIN_SRC" ]]; then
    echo "Build the release binary first: cargo build --release -p server-deck" >&2
    exit 1
fi

echo ">> Installing /usr/local/bin/server-deck..."
install -m 755 "$BIN_SRC" /usr/local/bin/server-deck

UNIT_SRC="$(dirname "$0")/../crates/server-deck/scripts/network-deck-server.service"
echo ">> Installing systemd unit..."
install -m 644 "$UNIT_SRC" /etc/systemd/system/network-deck-server.service
systemctl daemon-reload

echo
echo "Done. Next steps:"
echo "  1. On Windows, run 'client-win pair' (or use the tray app's Pair menu)."
echo "  2. Stop the service and run 'sudo /usr/local/bin/server-deck pair --state-dir /var/lib/network-deck' on the Deck while it's in pair mode."
echo "  3. Once paired: 'sudo systemctl enable --now network-deck-server.service'"
