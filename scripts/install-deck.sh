#!/usr/bin/env bash
# Deck-side installer. Idempotent.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

if [[ "$EUID" -ne 0 ]]; then
    echo "Run as root: sudo $0" >&2
    exit 1
fi

if ! command -v usbip >/dev/null; then
    if command -v steamos-readonly >/dev/null; then
        echo ">> SteamOS detected. Disabling readonly + installing usbip..."
        steamos-readonly disable
        trap 'steamos-readonly enable' EXIT
        if [[ ! -f /etc/pacman.d/gnupg/trustdb.gpg ]]; then
            pacman-key --init
            pacman-key --populate
        fi
        pacman -S --noconfirm usbip
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

BIN_SRC="$SCRIPT_DIR/../target/release/server-deck"
if [[ ! -f "$BIN_SRC" ]]; then
    echo "Build the release binary first: cargo build --release -p server-deck" >&2
    exit 1
fi

echo ">> Installing /usr/local/bin/server-deck..."
install -m 755 "$BIN_SRC" /usr/local/bin/server-deck

UNIT_SRC="$SCRIPT_DIR/../crates/server-deck/scripts/network-deck-server.service"
echo ">> Installing systemd unit..."
install -m 644 "$UNIT_SRC" /etc/systemd/system/network-deck-server.service
systemctl daemon-reload

KIOSK_SRC="$SCRIPT_DIR/../target/release/network-deck-kiosk"
if [[ ! -f "$KIOSK_SRC" ]]; then
    echo "Build the release binary first: cargo build --release -p kiosk-deck" >&2
    exit 1
fi

echo ">> Installing /usr/local/bin/network-deck-kiosk..."
install -m 755 "$KIOSK_SRC" /usr/local/bin/network-deck-kiosk

echo ">> Installing tmpfiles.d entry for /run/network-deck..."
install -m 644 "$SCRIPT_DIR/network-deck.tmpfiles" /etc/tmpfiles.d/network-deck.conf
systemd-tmpfiles --create /etc/tmpfiles.d/network-deck.conf

echo ">> Installing kiosk .desktop entry..."
install -m 644 "$SCRIPT_DIR/network-deck-kiosk.desktop" /usr/share/applications/network-deck-kiosk.desktop

echo
echo "Done. Next steps:"
echo "  1. On Windows, run 'client-win pair' (or use the tray app's Pair menu)."
echo "  2. Stop the service and run 'sudo /usr/local/bin/server-deck pair --state-dir /var/lib/network-deck' on the Deck while it's in pair mode."
echo "  3. Once paired: 'sudo systemctl enable --now network-deck-server.service'"
echo
echo "To add the kiosk to Game Mode:"
echo "  1. Reboot to Desktop Mode (Power → Switch to Desktop)."
echo "  2. Open Steam (the desktop client)."
echo "  3. Games → Add a Non-Steam Game to My Library..."
echo "  4. Browse to /usr/local/bin/network-deck-kiosk → Add Selected Programs."
echo "  5. Switch back to Game Mode; \"Network Deck\" appears in your library."
echo "  6. Tap it from Game Mode whenever you want to pause/resume controller sharing."
