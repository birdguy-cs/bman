# bman documentation

Everything in detail. For a quick start see the [README](README.md).

- [Music library](#music-library): [naming](#naming), [cover art](#cover-art), [playlists](#playlists)
- [On-screen keyboard](#on-screen-keyboard)
- [Using it over SSH (terminal driver)](#using-it-over-ssh-terminal-driver)
- [Configuration](#configuration): [screen timeout](#screen-timeout), [brightness](#brightness)
- [Network (Wi-Fi)](#network-wi-fi)
- [Bluetooth](#bluetooth)
- [Battery](#battery)
- [System: updates and start on boot](#system-updates-and-start-on-boot)
- [Hardware](#hardware)
- [Building](#building)
- [Making a release](#making-a-release)
- [Adding drivers](#adding-drivers)
- [Source layout](#source-layout)

## Music library

bman scans a folder called `music` next to where the program is run. Symlinks are followed, so `music` can point somewhere else. It must be a folder, not a file. The library is rescanned every time you change screen, so files you copy onto the device show up without restarting.

Supported formats: `mp3`, `wav`, `flac`, `ogg`, `aiff`, `aif`.

### Naming

Artist and album come from the names of folders and files:

```
music/
  Artist - Album/                          an album (folder named "Artist - Album")
    Artist - Album - 01 Song Title.aiff    track number comes after the album name
    Artist - Album - 02 Another Song.aiff
    cover.jpg                              optional cover art
  Artist - Single Song.mp3                 a single: "Artist - Title"
  My Playlist.m3u                          a playlist
```

`music/Artist/Album/` also works, and a folder with no " - " in its name is treated as an album with an unknown artist.

Singles and album folders can name the artist in any of these ways (`by` can be any case):

| Name | Artist | Title |
|---|---|---|
| `Artist - Title` (also `–` or `—`) | Artist | Title |
| `Title by Artist` / `Title, by Artist` | Artist | Title |
| `Title - by Artist` | Artist | Title |
| `Title (by Artist)` / `Title [by Artist]` | Artist | Title |
| `Title made by Artist` | Artist | Title |

Instead of `made`, these also work: `performed`, `sung`, `played`, `recorded`, `produced`, `written`, `composed`, `covered`, `remixed`, `mixed`, `arranged`. The last `by` counts, so `Stand by Your Man by Tammy Wynette` works, and titles like `Stand by Me` or `Day by Day` are left alone. Names with underscores instead of spaces (`Artist_-_Title`) are read as if they had spaces.

### Cover art

Embedded art in the song's tags is used first. If there is none, a `cover.jpg`, `cover.jpeg`, `cover.png` or `cover.bmp` in the song's folder is used. Art is shrunk to 96x96 pixels.

### Playlists

Playlists are standard `.m3u` / `.m3u8` files in the music folder, with one path per line (lines starting with `#` are comments). Paths inside the music folder are saved relative to it. Changes made on the device are written straight back to the file. Entries that cannot be found are usually path mismatches in the `.m3u`.

- **Create**: *+ New playlist* opens the [keyboard](#on-screen-keyboard) to name it; OK without typing anything keeps the default `Playlist N`.
- **Rename**: open the playlist and press 1 on **Rename playlist** (the first row). This renames the `.m3u` file; `/ \ : * ? " < > |` aren't allowed so the name works on any file system.
- **From the queue**: the first row of the Queue, **Save as playlist**, saves the whole queue as a new playlist and asks for a name.
- **Add / remove songs**: 5 presses on a song adds it to the selected playlist (or the queue); 5 presses inside a playlist removes it.

## On-screen keyboard

Used for Wi-Fi passwords, playlist names and the sudo password (start on boot, restart, shut down, unblocking a radio).

- 2 / 3: next / previous key. 1: type it. 5: delete the last character. 4: cancel.
- Four layouts: `abc`, `ABC`, `123` and symbols (`#+=`). The current one is shown under the text box.
- Every layout ends with **spc** (space), **OK** and the switch key, which is labelled with the layout it switches to. The list wraps, so from the first letter press 3 once for the switch key, twice for OK, three times for space.
- Pressing 1 on the switch key keeps the selection on it, so repeated presses cycle through the layouts.
- The sudo password is shown as stars except the last character typed.

## Using it over SSH (terminal driver)

No screen yet, or no GUI? Run

```
./bman --terminal
```

(or `-t`) and the screen is drawn right in the terminal with coloured blocks, over SSH too. Music still plays out of the Pi.

`--terminal` is a mirror: the display from `driver=` in `config.txt` (the Whisplay, or the window) keeps showing the same picture, and both take input (keys in the terminal, the real button or mouse on the other). If that display can't start (no Whisplay attached, no desktop for the window), bman says so and uses the terminal alone. `driver=terminal` in `config.txt` (or `--driver terminal`) means the terminal only.

| Key | Button |
|---|---|
| `1`, Enter, Space, `l`, → | 1 press: select / play / pause |
| `2`, ↓, `j`, `s` | 2 presses: down |
| `3`, ↑, `k`, `w` | 3 presses: up |
| `4`, ←, Backspace, Esc, `h`, `b` | 4 presses: back |
| `5`, `a`, `x` | 5 presses: action |
| `?` | hold: help |
| `-` / `+` (or `=`) | zoom out / in |
| `0` | zoom back to fit the window |
| `q`, Ctrl-C | quit |

- The picture starts shrunk to fit the window (full size needs a 240x141 character terminal; 120x71 shows it at half size, which is still readable). `-` / `+` zoom out / in from there in steps (100%, 80%, 67%, 57%, 50%, 40%, 33%, ... 12%), `0` goes back to fit. Zooming in past the window crops the right and bottom. The bottom line shows the zoom. Resizing the window is picked up straight away.
- Needs a terminal with true colour (any modern one: GNOME Terminal, Konsole, kitty, Alacritty, Windows Terminal, iTerm2, tmux with true colour on).
- While it runs, log messages go to `bman.log` in the current folder instead of the terminal.
- On the Pi the real button still works alongside the keys.
- It needs an interactive terminal, so it can't be used by the boot service; that one still uses `driver` from `config.txt`.

## Configuration

Settings live in `config.txt` next to the program, one `key=value` per line. The settings screen and the device rewrite this file, so it may be re-ordered.

| Key | Values | Default | Meaning |
|---|---|---|---|
| `driver` | `whisplay`, `window`, `terminal` | `whisplay` | Which display driver to use. `window` is the PC test window, `terminal` draws in the terminal (see [over SSH](#using-it-over-ssh-terminal-driver)). `bman --driver NAME` overrides it for one run without changing `config.txt`; `bman --terminal` also mirrors it into the terminal. |
| `battery` | `auto`, `none`, `sysfs`, `pisugar` | `auto` | Which battery driver to use (see [Battery](#battery)). |
| `volume` | 0.0 and up | `1.0` | Music volume: bman's own level (see [Volume](#volume)). |
| `system_volume` | 0-100 (percent) | not set | System (output device) volume, set from Settings > Volume. Re-applied at startup and when Bluetooth headphones connect. Not set = bman leaves the system volume alone. |
| `use_system_audio` | `true` / `false` | `false` | `true` plays through the normal system audio device (rodio), for testing on a PC. `false` plays through the GPIO PWM output on the Pi. |
| `accent` | `R,G,B` (0-255) | `42,111,219` | Main UI colour. |
| `output` | `wired` / `wireless` | `wired` | Wireless uses system audio (Bluetooth). |
| `screen_timeout` | 30-300 (seconds), or `0` | `60` | Idle time before the screen sleeps. `0` (or anything under 30) = never. |
| `brightness` | 10-100 (percent) | `100` | Screen brightness. |
| `wifi` | `SSID,password` | none | A Wi-Fi network to save at startup (see [Network](#network-wi-fi)). Repeat the line for more networks. |
| `update_check` | `true` / `false` | `true` | Check GitHub for a new version at startup and every minute, and ask to install it (see [System](#system-updates-and-start-on-boot)). |

Example:

```
driver=whisplay
battery=auto
volume=0.8
use_system_audio=false
accent=42,111,219
output=wired
screen_timeout=60
brightness=100
update_check=true
wifi=My Home Wi-Fi,hunter22
```

For a PC test window, set `driver=window` and `use_system_audio=true`.

### Volume

Settings > Volume (the first row shows both, `M` music and `S` system) opens two levels. Press 1 on one to edit it, 2 / 3 to go down / up in 5% steps, 1 when done.

- **Music**: bman's own level, applied to whatever it plays (wired or Bluetooth).
- **System**: the output device's volume, e.g. Bluetooth earbuds. It's set through the ALSA mixer of the default device (`Master`), which with PipeWire (`pipewire-alsa`) or PulseAudio is the sound server's current output: the same volume `wpctl set-volume` changes. Shows *n/a* when there's no mixer.

Both are saved in `config.txt` and survive reboots. The system level is put back at startup (retrying until the sound server is up) and again a couple of seconds after Bluetooth headphones connect, so they don't come back at whatever volume they last had. Until you change System once, bman doesn't touch the system volume.

### Screen timeout

After the timeout with no input, the screen turns off. Music keeps playing. In Settings > Sleep, going below 30s gives **Never**: the screen stays on. The first press wakes it; if a song is playing, that press also acts on Now Playing (so 1 press pauses or plays).

### Brightness

On the Whisplay the backlight is dimmed with software PWM down to a flicker-free floor (50%). Anything dimmer is done by scaling the picture's colours. Drivers without a dimmable backlight (the PC window) do all of it with colour. The constants `BACKLIGHT_MIN_PCT` and `BACKLIGHT_PWM_HZ` at the top of `whisplay.rs` are the tuning knobs.

## Network (Wi-Fi)

Settings > Network shows Wi-Fi (through NetworkManager, the default on Raspberry Pi OS Bookworm, Ubuntu, Debian, Pop!_OS, Fedora, Manjaro and SteamOS):

- **Wi-Fi: On/Off**: press 1 to switch the radio on or off.
- **IP**: the current IP address (handy for SSH). Press 1 to show it as a popup.
- **Scan for networks**, then the networks: `*` connected, `+` saved, with signal strength. Press 1 on one to connect (or disconnect if it's connected). 5 presses on a saved network forgets it.

A secured network that isn't saved opens the [keyboard](#on-screen-keyboard) for the password. A wrong password isn't saved. Passwords typed here are stored only by NetworkManager (root-only files in `/etc/NetworkManager/system-connections/`).

To skip typing, add `wifi=SSID,password` lines to `config.txt` (one per network, empty password for an open network). They're saved in NetworkManager at startup and it connects on its own. These are plain text in `config.txt`. SSIDs containing a comma aren't supported. WPA-Enterprise (username + password) networks aren't listed. If NetworkManager isn't running the screen shows *No network manager*.

## Bluetooth

Pair devices from Settings > Bluetooth. bman talks to BlueZ (`bluetoothd`) directly over D-Bus, so `bluetoothctl` isn't needed.

- Press 1 on *Scan*, then 1 on a device to pair, trust and connect it. 1 on a connected device disconnects it.
- When headphones connect, audio switches to `wireless` by itself and the song carries on there. If they disconnect, playback pauses and audio goes back to `wired`.
- Trusted devices reconnect on their own.
- **Stutters**: playback decodes about 1.5 s ahead on its own thread, and background work runs at the lowest priority, so the Pi being busy shouldn't cause hiccups. But on a Pi Zero 2 W, Wi-Fi and Bluetooth share one radio: heavy Wi-Fi traffic (copying music over, a busy SSH session) can still make Bluetooth audio stutter. The terminal driver limits itself to 10 updates a second for this reason.
- **Blocked radio** (`PowerState: off-blocked` in `bluetoothctl show`, common on a fresh Raspberry Pi OS): bman unblocks it itself. At startup it tries quietly (works with passwordless sudo); otherwise opening Settings > Bluetooth asks for the sudo password on the keyboard. Same for Wi-Fi when you switch it on in Settings > Network. A hardware block (a physical switch) can't be lifted by software and shows *off by hardware switch*.

Needs `bluetoothd` running plus an audio server with Bluetooth support running as your user: PipeWire with WirePlumber (and its BlueZ/SPA plugin), or PulseAudio with `pulseaudio-module-bluetooth`. Run only one of them; having both PulseAudio and `pipewire-pulse` running breaks Bluetooth audio. If connecting says *No BT audio service*, the audio server isn't running or lacks Bluetooth support.

## Battery

A battery icon and percentage sit at the top right of every screen. The driver can be set in `config.txt` or switched from the **Battery** row in Settings (press 1 to cycle through the drivers; it applies immediately).

While on battery, a warning pops up at 15% ("Battery low") and 5% ("Battery critical"). If the screen is off it doesn't wake it; the warning shows the next time you press the button. Plugging in resets the warnings. While charging (or plugged in and full) the icon turns green and shows a lightning bolt.

- `battery=auto` (default) uses `sysfs` if the kernel reports a battery, otherwise `pisugar` if the server answers, otherwise shows the infinity symbol. Re-checked every 10 s, so a battery that shows up later is picked up.
- `battery=none` always shows a full battery with an infinity symbol. Use it when there is no battery to read, or when the battery module can't report its level (the PiSugar S can't).
- `battery=pisugar` reads the level and charging state from PiSugar's power manager (`pisugar-server`, e.g. for a PiSugar 2 or 3). It must be installed and running. The driver talks to `/tmp/pisugar.sock`, falling back to TCP `127.0.0.1:8423`. A `?` means it could not reach the server.
- `battery=sysfs` reads the standard Linux battery interface (`/sys/class/power_supply`). Works on laptops and with any battery HAT or fuel gauge that has a kernel driver. Peripheral batteries (mice, headsets) are ignored; multiple batteries are combined. A `?` means no battery was found.

## System: updates and start on boot

The **System** row in Settings shows the running version (or `update!`) and opens:

- **Check for updates** / **Install vX.Y.Z**: press 1 to check GitHub now; once a newer release is found, press 1 again to download it, check its SHA-256 and replace the program file. If a song is playing, bman keeps playing and restarts into the new version when the song changes (it ends, or you skip or pick another), then carries on with the queue: same next song, same loop mode. The top bar says *Update ready: restarts after this song*, and this row becomes **Restart now (update ready)** if you don't want to wait. With nothing playing (or paused) it restarts right away and resumes where you were.
- **Auto check: On/Off** (`update_check` in `config.txt`): checks at startup and then every minute. The check is one small request to github.com, not the rate-limited API. When a new version is out, an **Update?** screen asks *Update now* / *Not now*. If the screen is asleep it wakes up to ask (it only waits while you're typing on the keyboard), and *Not now* (or 4) isn't asked again until the next start. The screen doesn't go to sleep while the prompt is up, while Settings > System is offering an update, or while an update downloads and installs. (The prompt for 0.2.0 was shown by 0.1.5, which didn't have this yet.)
- **Start on boot: On/Off**: see [below](#start-on-boot).
- **Restart device** / **Shut down**: press 1 twice (the row says *Press 1 again* for 3 s, so one stray press does nothing). See [Restart and shut down](#restart-and-shut-down).
- **Restart bman**: restarts just the player, carrying on the song and queue from where they were.

### Updates

- Needs internet, and write access to the folder the `bman` binary is in (run it from a folder your user owns, not a root-owned `/usr/bin`).
- Prebuilt releases exist for x86_64, 64-bit Pi (aarch64) and 32-bit Pi (armv7), glibc 2.36 or newer (Raspberry Pi OS Bookworm, Debian 12, Ubuntu 22.10 and newer, any current Arch/Manjaro/Fedora/Void-glibc). musl systems show *No builds for this system*; build from source there.
- A self-built binary updates too: it's replaced by the release build once a newer version comes out.

### Start on boot

Writes a boot service for the running init system that starts bman from the current folder as your user. It only writes files, no init commands, so it takes effect at the next boot. Turning it off removes only what bman wrote.

Writing into `/etc` needs root, so bman runs itself through `sudo` just to write the files. With passwordless sudo (the Raspberry Pi OS default) that just works; otherwise the [keyboard](#on-screen-keyboard) asks for your sudo password (a wrong one asks again, 4 cancels). The password is passed straight to `sudo` and never saved. Without sudo, passwordless `doas` is tried. The service itself always runs as you, not root. If it can't be done, an error pops up (*Not allowed to use sudo*, *System files are read-only*, *Unknown init system*, ...).

| Init (detected from PID 1) | Default on | What's written |
|---|---|---|
| systemd | Raspberry Pi OS, Debian, Ubuntu, Pop!_OS, Mint, Fedora, Arch, Manjaro, EndeavourOS, SteamOS, openSUSE | `/etc/systemd/system/bman.service` + enable link; also enables lingering so PipeWire (Bluetooth audio) starts without logging in |
| OpenRC | Gentoo, Alpine, Artix-OpenRC | `/etc/init.d/bman` + `runlevels/default` link |
| runit | Void, Artix-runit, Devuan-runit | `run` script in `/etc/sv` (Artix: `/etc/runit/sv`) + service link |
| dinit | Chimera, Artix-dinit | `/etc/dinit.d/bman` + `boot.d` link |
| sysvinit / BSD-style | Devuan, MX Linux, antiX, Slackware | a marked block in `/etc/rc.local` (Slackware: `/etc/rc.d/rc.local`) |

s6 (Artix-s6, Obarun) needs its own tools to enable a service, so it shows *Unknown init system*. NixOS has a read-only `/etc`; add bman to your configuration instead.

### Restart and shut down

bman first asks the login manager over D-Bus (logind from systemd or elogind, or ConsoleKit2 on Devuan). By default that only allows someone at the device's own screen, not an SSH session or the boot service, so the first time bman falls back to sudo (passwordless sudo just works; otherwise the keyboard asks for the sudo password). As root it runs the system's own `reboot` / `poweroff`, so the shutdown is clean on any init, and it also installs a polkit rule, `/etc/polkit-1/rules.d/50-bman-power.rules`, that lets your user restart and shut down (nothing else) without a password. From then on it never asks again. Turning on Start on boot installs the rule too. It needs polkit (`ls /etc/polkit-1/rules.d`; on a minimal Debian / Raspberry Pi OS: `sudo apt install polkitd`). Delete the file to undo. Settings are saved first.

## Hardware

- **Board**: [Raspberry Pi Zero 2 W](https://www.amazon.com/dp/B09LH5SBPS).
- **Screen**: [PiSugar Whisplay HAT](https://www.amazon.com/dp/B0FPG8S6K6) (ST7789, 240x280). The built-in mono audio is not used.
- **Battery**: [PiSugar 3](https://www.amazon.com/dp/B0FB3N1YSK). Install and run `pisugar-server` so the level and charging state show up (`battery=auto` finds it). See [Battery](#battery).
- **Button**: GPIO 17. Wiring polarity is detected at startup, so don't hold the button while launching.
- **Audio**: stereo PWM on GPIO 12 and 13 through a low-pass filter. Add this to `/boot/firmware/config.txt`:

  ```
  dtoverlay=pwm-2chan,pin=12,func=4,pin2=13,func2=4
  ```

  For the headphone jack I use a [3.5 mm TRS stereo jack breakout board](https://www.amazon.com/dp/B0H6F3PZGK).

The Whisplay driver was written from the published pin layout without the vendor's reference code, so the init sequence, backlight polarity (`BACKLIGHT_ACTIVE_LOW`), SPI speed and orientation are best guesses and may need adjusting on real hardware.

## Building

```
cargo run --release
```

Building needs Rust, `pkg-config` and the ALSA development headers (rodio uses ALSA, which PipeWire and PulseAudio also provide through their ALSA plugins):

| Distro | Packages |
|---|---|
| Debian, Ubuntu, Pop!_OS, Raspberry Pi OS | `sudo apt install pkg-config libasound2-dev` |
| Arch, Manjaro, SteamOS | `sudo pacman -S --needed base-devel alsa-lib` |
| Fedora | `sudo dnf install pkgconf-pkg-config alsa-lib-devel` |
| Void | `sudo xbps-install pkg-config alsa-lib-devel` |
| Gentoo | `media-libs/alsa-lib` (headers included) |

At runtime the binary only needs `libasound.so.2` and libc, which every desktop and Raspberry Pi OS install already has. X11/Wayland libraries are loaded only when the PC window driver is used. Nothing depends on the init system: Bluetooth only needs the system D-Bus and `bluetoothd`, the updater uses pure-Rust HTTPS with built-in root certificates, and the battery uses sysfs or the PiSugar socket. (SteamOS has a read-only root, so building there is easiest in a distrobox or on another machine.)

## Making a release

Bump `version` in `Cargo.toml`, commit, then:

```
git tag vX.Y.Z && git push origin vX.Y.Z
```

Versioning: small fixes and additions bump the patch number (`0.1.4` → `0.1.5`); a bigger update bumps the minor number (`0.1.5` → `0.2.0`). The patch number never goes past 9: the release after `0.1.9` is `0.2.0`, not `0.1.10` (the workflow refuses a patch above 9). Release notes are written from the commit messages by `.github/release-notes.sh`: a patch release lists just its own changes, and an `x.y.0` release lists everything since the previous `x.y.0`, grouped by version, so it reads as the full update.

The GitHub Actions workflow (`.github/workflows/release.yml`) builds all three targets in a Debian 12 container, attaches `bman-<target>` and `bman-<target>.sha256` to the release and publishes it. The tag must match the `Cargo.toml` version or the build fails, which stops a release that would offer itself as an update forever.

## Adding drivers

Displays and batteries are both pluggable:

- **Display**: implement the `Display` trait (see `whisplay.rs`), add the name to `DRIVERS` in `display_driver.rs`, and add a match arm in `create_display`.
- **Battery**: implement `BatterySource` (see `battery.rs`), and add one `(name, constructor)` line to `BATTERY_DRIVERS`. Config and the Settings row pick it up automatically.

## Source layout

| File | What it does |
|---|---|
| `main.rs` | App state, navigation, button handling, main loop |
| `render.rs` | Drawing every screen, the `Display` trait |
| `display_driver.rs` | Display driver selection, PC window driver |
| `whisplay.rs` | Whisplay HAT screen driver, backlight |
| `battery.rs` | Battery drivers and monitor |
| `buttons.rs` | Single-button press counting, GPIO button thread |
| `audio.rs` | Playback (rodio, or GPIO PWM on the Pi) |
| `library.rs` | Scanning, albums, artists, playlists |
| `art.rs` | Cover art loading |
| `network.rs` | Wi-Fi through NetworkManager |
| `bluetooth.rs` | Bluetooth pairing and connection |
| `update.rs` | Self-updater (GitHub releases) |
| `autostart.rs` | Start on boot (systemd, OpenRC, runit, dinit, rc.local) |
| `power.rs` | Restart / shut down (logind or ConsoleKit over D-Bus, else sudo) |
| `root.rs` | Running a job as root through sudo (start on boot, power, rfkill) |
| `rfkill.rs` | Reading and lifting rfkill blocks on Bluetooth / Wi-Fi |
| `config.rs` | `config.txt` loading and saving |
| `resume.rs` | Carrying the queue across a restart (update, Restart bman) |
| `sched.rs` | Thread priorities (background work lowest, audio highest allowed) |
| `terminal.rs` | Terminal display driver (for SSH) |
| `log.rs` | Coloured log output |
