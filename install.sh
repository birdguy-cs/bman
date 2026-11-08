#!/bin/sh
# bman installer / uninstaller.
#
# Install or update (into ~/bman, start script at ~/start-bman.sh):
#   curl -fsSL https://github.com/birdguy-cs/bman/releases/latest/download/install.sh | sh
#   (or: wget -qO- .../install.sh | sh)
#
# Uninstall (asks before deleting your music; a copy of this script is kept
# as ~/bman/install.sh):
#   sh ~/bman/install.sh --uninstall
#   curl -fsSL .../install.sh | sh -s -- --uninstall
#
# Options:
#   --uninstall   remove bman (start on boot, polkit rule, start script, program, streaming tools and login)
#   --purge       with --uninstall: also delete ~/bman (music, playlists, settings) without asking
#   --no-sudo     never use sudo (skips the polkit rule / start-on-boot cleanup)
#
# Running the installer again updates the program; your music, config.txt
# and playlists are never touched. Set BMAN_DIR to use a folder other than ~/bman.

set -eu

REPO="birdguy-cs/bman"
BMAN_DIR="${BMAN_DIR:-$HOME/bman}"
START_SCRIPT="$HOME/start-bman.sh"
BASE="https://github.com/$REPO/releases/latest/download"
POLKIT_DIR="/etc/polkit-1/rules.d"
POLKIT_RULE="$POLKIT_DIR/50-bman-power.rules"
MARKER="Written by bman"

say() { printf '%s\n' "$*"; }
fail() { printf 'bman install: %s\n' "$*" >&2; exit 1; }

UNINSTALL=false
PURGE=false
NO_SUDO=false
for arg in "$@"; do
    case "$arg" in
        --uninstall) UNINSTALL=true ;;
        --purge) PURGE=true ;;
        --no-sudo) NO_SUDO=true ;;
        -h|--help) sed -n '2,22p' "$0" 2>/dev/null || say "See https://github.com/$REPO/blob/main/DOCS.md#installing"; exit 0 ;;
        *) fail "unknown option $arg (try --help)" ;;
    esac
done

# Questions are read from the terminal, so they work with `curl ... | sh`.
# Without a terminal the default answer is used.
ask() { # ask "Question?" default(y/n)
    if (: < /dev/tty > /dev/tty) 2>/dev/null; then
        printf '%s ' "$1" > /dev/tty
        read -r answer < /dev/tty || answer=""
    else
        answer=""
    fi
    [ -z "$answer" ] && answer="$2"
    case "$answer" in y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
}

can_sudo() {
    [ "$NO_SUDO" = false ] && command -v sudo >/dev/null 2>&1
}

# --- uninstall ----------------------------------------------------------------

if [ "$UNINSTALL" = true ]; then
    say "Uninstalling bman from $BMAN_DIR"
    if command -v pgrep >/dev/null 2>&1 && pgrep -x bman >/dev/null 2>&1; then
        say "Note: bman is running. Quit it (or reboot) after this."
    fi

    if can_sudo; then
        # bman's own root helper removes its boot service (only files it
        # wrote). Only when one of its enable links / rc.local blocks exists.
        boot=false
        for link in /etc/systemd/system/multi-user.target.wants/bman.service /etc/runlevels/default/bman \
                    /var/service/bman /etc/runit/runsvdir/default/bman /etc/service/bman /etc/dinit.d/boot.d/bman; do
            if [ -e "$link" ] || [ -L "$link" ]; then boot=true; fi
        done
        if grep -qs ">>> bman" /etc/rc.local /etc/rc.d/rc.local; then boot=true; fi
        if [ "$boot" = true ] && [ -x "$BMAN_DIR/bman" ]; then
            say "Turning off start on boot (needs sudo)..."
            sudo "$BMAN_DIR/bman" --autostart off 2>/dev/null | sed 's/^OK /  /; s/^ERR /  skipped: /' || true
        fi
        if sudo test -e "$POLKIT_RULE"; then
            if sudo grep -q "$MARKER" "$POLKIT_RULE"; then
                sudo rm -f "$POLKIT_RULE" && say "Removed $POLKIT_RULE"
            else
                say "Left $POLKIT_RULE alone (not written by bman)"
            fi
        fi
    else
        say "Skipping start-on-boot and polkit cleanup (no sudo)."
    fi

    if [ -f "$START_SCRIPT" ] && grep -q "bman" "$START_SCRIPT"; then
        rm -f "$START_SCRIPT" && say "Removed $START_SCRIPT"
    fi

    if [ -d "$BMAN_DIR" ]; then
        rm -f "$BMAN_DIR/bman" "$BMAN_DIR/bman.log" "$BMAN_DIR/.bman-resume" \
              "$BMAN_DIR/.bman-update.tmp" "$BMAN_DIR/.bman-download" "$BMAN_DIR/.bman-download.sha256" \
              "$BMAN_DIR/config.txt.tmp"
        rm -rf "$BMAN_DIR/tools" "$BMAN_DIR/temp"   # yt-dlp / Deno and streamed songs
        rm -f "$BMAN_DIR/youtube-cookies.txt"       # YouTube login (liked.txt stays with your music)
        say "Removed the bman program from $BMAN_DIR"
        if [ "$PURGE" = true ] || ask "Also delete $BMAN_DIR with your music, playlists and settings? [y/N]" n; then
            rm -rf "$BMAN_DIR" && say "Deleted $BMAN_DIR"
        else
            rm -f "$BMAN_DIR/install.sh"
            say "Kept your music, playlists and config.txt in $BMAN_DIR"
        fi
    fi

    say ""
    say "bman is uninstalled. Left as they were: system packages (PipeWire etc.),"
    say "user lingering (sudo loginctl disable-linger $(id -un) to undo) and the"
    say "wired-audio line in /boot/firmware/config.txt, if you added it."
    exit 0
fi

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

# A copy of this script, for `sh ~/bman/install.sh --uninstall` later.
fetch "$BASE/install.sh" "$BMAN_DIR/install.sh" 2>/dev/null && chmod 755 "$BMAN_DIR/install.sh" || true

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

# --- restart / shut down without a password ------------------------------------
# Over SSH or as a boot service the login manager won't let a normal user
# restart without a password. A polkit rule allows just this user, just
# restart and shut down (the same rule bman installs itself the first time
# it has to use sudo for that).

user="$(id -un)"
already_allowed=false
if command -v busctl >/dev/null 2>&1 && busctl call org.freedesktop.login1 /org/freedesktop/login1 \
        org.freedesktop.login1.Manager CanReboot 2>/dev/null | grep -q '"yes"'; then
    already_allowed=true
fi

if [ "$already_allowed" = false ] && [ "$user" != "root" ]; then
    if [ ! -d "$POLKIT_DIR" ]; then
        say ""
        say "Note: polkit isn't installed, so restarting from bman will ask for the sudo password."
        say "  (Debian / Raspberry Pi OS: sudo apt install polkitd, then run this installer again.)"
    elif can_sudo; then
        say ""
        say "Letting $user restart / shut down from bman without a password (polkit rule, sudo once)..."
        rule="$BMAN_DIR/.bman-polkit.rules"
        cat > "$rule" <<EOF
// $MARKER: lets $user restart / shut down without a password
// (Settings > System). Delete this file to undo.
polkit.addRule(function(action, subject) {
    var power = [
        "org.freedesktop.login1.reboot",
        "org.freedesktop.login1.reboot-multiple-sessions",
        "org.freedesktop.login1.power-off",
        "org.freedesktop.login1.power-off-multiple-sessions",
        "org.freedesktop.consolekit.system.restart",
        "org.freedesktop.consolekit.system.restart-multiple-users",
        "org.freedesktop.consolekit.system.stop",
        "org.freedesktop.consolekit.system.stop-multiple-users"
    ];
    if (power.indexOf(action.id) >= 0 && subject.user == "$user") {
        return polkit.Result.YES;
    }
});
EOF
        if sudo test -e "$POLKIT_RULE" && ! sudo grep -q "$MARKER" "$POLKIT_RULE"; then
            say "  $POLKIT_RULE exists and isn't bman's; left it alone."
        elif sudo install -m 644 "$rule" "$POLKIT_RULE"; then
            say "  Done."
        else
            say "  Skipped (bman will ask for the sudo password the first time you restart from it)."
        fi
        rm -f "$rule"
    fi
fi

# --- what's next ------------------------------------------------------------------

version="$("$BMAN_DIR/bman" --version 2>/dev/null || true)"
say ""
say "Done${version:+: $version}."
say "  Program:   $BMAN_DIR/bman (updates itself from Settings > System)"
say "  Music:     put songs in $BMAN_DIR/music"
say "  Start:     ~/start-bman.sh   (add --terminal to see it over SSH)"
say "  Uninstall: sh $BMAN_DIR/install.sh --uninstall"

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
