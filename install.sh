#!/bin/sh
# bman installer. Downloads the latest release for this machine into ~/bman
# and puts a start script at ~/start-bman.sh.
#
#   curl -fsSL https://github.com/birdguy-cs/bman/releases/latest/download/install.sh | sh
#   (or: wget -qO- .../install.sh | sh)
#
# Run it again to update the program; your music, config.txt and playlists
# are never touched. Set BMAN_DIR to install somewhere other than ~/bman.

set -eu

REPO="birdguy-cs/bman"
BMAN_DIR="${BMAN_DIR:-$HOME/bman}"
START_SCRIPT="$HOME/start-bman.sh"
BASE="https://github.com/$REPO/releases/latest/download"

say() { printf '%s\n' "$*"; }
fail() { printf 'bman install: %s\n' "$*" >&2; exit 1; }

# --- which build -------------------------------------------------------------

[ "$(uname -s)" = "Linux" ] || fail "bman runs on Linux only."

if ldd --version 2>&1 | grep -qi musl; then
    fail "this is a musl system; the prebuilt releases need glibc. Build from source instead (see DOCS.md#building)."
fi

# The userland decides, not the kernel: Raspberry Pi OS 32-bit often runs a
# 64-bit kernel, so check the word size too.
bits="$(getconf LONG_BIT 2>/dev/null || echo 64)"
case "$(uname -m)" in
    x86_64|amd64)
        asset="bman-x86_64-linux-gnu" ;;
    aarch64|arm64)
        if [ "$bits" = "32" ]; then asset="bman-armv7-linux-gnueabihf"; else asset="bman-aarch64-linux-gnu"; fi ;;
    armv7l|armv8l)
        asset="bman-armv7-linux-gnueabihf" ;;
    *)
        fail "no prebuilt bman for $(uname -m). Build from source instead (see DOCS.md#building)." ;;
esac

# --- tools ---------------------------------------------------------------------

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    fail "needs curl or wget to download bman."
fi

if command -v sha256sum >/dev/null 2>&1; then
    sha() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    fail "needs sha256sum (coreutils) to check the download."
fi

# --- download and check ---------------------------------------------------------

say "Installing bman ($asset) into $BMAN_DIR"
mkdir -p "$BMAN_DIR"
tmp="$BMAN_DIR/.bman-download"
trap 'rm -f "$tmp" "$tmp.sha256"' EXIT

fetch "$BASE/$asset" "$tmp" || fail "download failed ($BASE/$asset)."
fetch "$BASE/$asset.sha256" "$tmp.sha256" || fail "checksum download failed."

expected="$(cut -d' ' -f1 < "$tmp.sha256")"
actual="$(sha "$tmp")"
[ "$expected" = "$actual" ] || fail "checksum mismatch, download corrupted. Try again."

chmod 755 "$tmp"
mv -f "$tmp" "$BMAN_DIR/bman"   # atomic, safe even while bman is running

# --- folder and start script ------------------------------------------------------

mkdir -p "$BMAN_DIR/music"

# First install on something that isn't a Raspberry Pi: use the PC test window
# and the normal sound output instead of the Whisplay screen and GPIO audio.
is_pi=false
if grep -qi "raspberry pi" /proc/device-tree/model 2>/dev/null; then is_pi=true; fi
if [ ! -e "$BMAN_DIR/config.txt" ] && [ "$is_pi" = false ]; then
    printf 'driver=window\nuse_system_audio=true\n' > "$BMAN_DIR/config.txt"
    say "Not a Raspberry Pi: wrote config.txt for the PC window and normal sound output."
fi

cat > "$START_SCRIPT" <<EOF
#!/bin/sh
# Starts bman from its folder (so it finds music/ and config.txt there),
# passing along any options, e.g.:
#   ./start-bman.sh
#   ./start-bman.sh --terminal
#   ./start-bman.sh --driver window
# Written by the bman installer.

BMAN_DIR="\${BMAN_DIR:-$BMAN_DIR}"

if [ ! -x "\$BMAN_DIR/bman" ]; then
    echo "bman isn't installed in \$BMAN_DIR (run install.sh, or set BMAN_DIR)." >&2
    exit 1
fi

cd "\$BMAN_DIR" || exit 1
exec ./bman "\$@"
EOF
chmod 755 "$START_SCRIPT"

# --- what's next ------------------------------------------------------------------

version="$("$BMAN_DIR/bman" --version 2>/dev/null || true)"
say ""
say "Done${version:+: $version}."
say "  Program:  $BMAN_DIR/bman (updates itself from Settings > System)"
say "  Music:    put songs in $BMAN_DIR/music"
say "  Start:    ~/start-bman.sh   (add --terminal to see it over SSH)"

if ! ldconfig -p 2>/dev/null | grep -q 'libasound.so.2' \
    && ! ls /usr/lib/libasound.so.2 /usr/lib/*/libasound.so.2 /lib/*/libasound.so.2 >/dev/null 2>&1; then
    say ""
    say "Note: libasound (ALSA) wasn't found. Install it first, e.g."
    say "  Debian / Ubuntu / Raspberry Pi OS: sudo apt install libasound2   (libasound2t64 on newer)"
    say "  Arch / Manjaro:                    sudo pacman -S alsa-lib"
    say "  Fedora:                            sudo dnf install alsa-lib"
fi

if [ "$is_pi" = true ]; then
    say ""
    say "Raspberry Pi setup (once), see DOCS.md#hardware:"
    say "  - Wired audio: add to /boot/firmware/config.txt and reboot:"
    say "      dtoverlay=pwm-2chan,pin=12,func=4,pin2=13,func2=4"
    say "  - Bluetooth headphones: sudo apt install pipewire wireplumber libspa-0.2-bluetooth pipewire-alsa"
    say "    (see DOCS.md#bluetooth for using them without a desktop)"
fi
