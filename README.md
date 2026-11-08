## AI Usage Notice

Yes, this repository uses AI-generated assistance, but with heavy human supervision. I'm mainly using AI because I'm too lazy to write some of this shit myself. I'd much rather spend my time writing games and other projects. The AI's used are Claude and ChatGPT.

The code, decisions, and overall direction are still human-supervised.

# bman

A lightweight music player built to run on the **Raspberry Pi Zero 2 W**, controlled by a **single button**. It plays MP3, WAV, FLAC, OGG and AIFF, and is designed around a small 240x280 screen (the PiSugar Whisplay HAT). You can also run it on a PC in a window for testing, where mouse clicks act as the button.

## Controls

There is one button. What it does depends on how many times you press it quickly (within about 0.35 s of each other).

| Presses | Action |
|---|---|
| 1 | Select / play / pause |
| 2 | Move down |
| 3 | Move up (on the Now Playing screen: change loop mode) |
| 4 | Back |
| 5 | Action (add the highlighted song to the queue or the selected playlist; remove it when you are in a playlist or the queue) |
| Hold (about 0.8 s) | Help screen |

Music can be started from any screen. When the screen is asleep (see [Screen timeout](#screen-timeout)), the first press wakes it. If a song is playing, that same press also acts on Now Playing (so 1 press pauses or plays).

## Screens

- **Singles**: songs sitting directly in the music folder.
- **Albums**: folders of songs, showing the artist and track list.
- **Artists**: browse artist, then album, then single.
- **Playlists**: your `.m3u` files. Create, name, rename, add to and remove from them.
- **Queue**: what is playing and what is next. Treated like a playlist. The first row, **Save as playlist**, saves the whole queue as a new playlist and asks for a name.
- **Now Playing**: cover art, title, artist, album, progress and loop mode.
- **Settings**: volume, accent colour, audio output, brightness, sleep timer, battery driver, network (Wi-Fi), Bluetooth, system (updates, start on boot).

Long titles scroll when they don't fit. The library is rescanned every time you change screen, so files you copy onto the device show up without restarting.

## Setting up your music

bman scans a folder called `music` next to where the program is run. Symlinks are followed, so `music` can point somewhere else. It must be a folder, not a file.

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

Creating a playlist (*+ New playlist*) opens the on-screen keyboard to name it; pressing OK without typing anything keeps the default `Playlist N`. To rename one later, open it and press 1 on **Rename playlist** (the first row). The keyboard works like the Wi-Fi password one: 2 / 3 step through characters, 1 types, 5 deletes, OK (press 3 once from the start) saves, 4 cancels. Renaming renames the `.m3u` file; `/ \ : * ? " < > |` aren't allowed so the name works on any file system.

## Configuration

Settings live in `config.txt` next to the program, one `key=value` per line. The settings screen and the device rewrite this file, so it may be re-ordered.

| Key | Values | Default | Meaning |
|---|---|---|---|
| `driver` | `whisplay`, `window` | `whisplay` | Which display driver to use. `window` is the PC test window. |
| `battery` | `auto`, `none`, `sysfs`, `pisugar` | `auto` | Which battery driver to use (see [Battery](#battery)). |
| `volume` | 0.0 and up | `1.0` | Playback volume. |
| `use_system_audio` | `true` / `false` | `false` | `true` plays through the normal system audio device (rodio), for testing on a PC. `false` plays through the GPIO PWM output on the Pi. |
| `accent` | `R,G,B` (0-255) | `42,111,219` | Main UI colour. |
| `output` | `wired` / `wireless` | `wired` | Wireless uses system audio (Bluetooth). |
| `screen_timeout` | 30-300 (seconds) | `60` | Idle time before the screen sleeps. |
| `brightness` | 10-100 (percent) | `100` | Screen brightness. |
| `wifi` | `SSID,password` | none | A Wi-Fi network to save at startup (see [Network](#network)). Repeat the line for more networks. |
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

### Screen timeout

After the timeout with no input, the screen turns off. Music keeps playing. Press the button to wake it.

### Brightness

On the Whisplay the backlight is dimmed with software PWM down to a flicker-free floor (50%). Anything dimmer is done by scaling the picture's colours. Drivers without a dimmable backlight (the PC window) do all of it with colour. The constants `BACKLIGHT_MIN_PCT` and `BACKLIGHT_PWM_HZ` at the top of `whisplay.rs` are the tuning knobs.

### Network

Settings > Network shows Wi-Fi (through NetworkManager, the default on Raspberry Pi OS Bookworm, Ubuntu, Debian, Pop!_OS, Fedora, Manjaro and SteamOS):

- **Wi-Fi: On/Off**: press 1 to switch the radio on or off.
- **IP**: the current IP address (handy for SSH). Press 1 to show it as a popup.
- **Scan for networks**, then the networks: `*` connected, `+` saved, with signal strength. Press 1 on one to connect (or disconnect if it's connected). 5 presses on a saved network forgets it.

A secured network that isn't saved opens the password screen: 2 / 3 step through the keys, 1 types the highlighted one, 5 deletes the last one, and 4 cancels. The keys come in four layouts, `abc`, `ABC`, `123` and symbols (`#+=`); each one ends with **spc** (space), **OK** and the switch key, which is labelled with the next layout. The switch key sits just before the first letter, so from the start press 3 once to switch layout, twice for OK, three times for space. Pressing 1 on the switch key keeps the selection on it, so repeated presses cycle through the layouts. The same keyboard is used for naming playlists and the sudo password. A wrong Wi-Fi password isn't saved.

To skip typing, add `wifi=SSID,password` lines to `config.txt` (one per network, empty password for an open network). They're saved in NetworkManager at startup and it connects on its own. SSIDs containing a comma aren't supported. WPA-Enterprise (username + password) networks aren't listed. If NetworkManager isn't running the screen shows *No network manager*.

### Battery

A battery icon and percentage sit at the top right of every screen. The driver can be set in `config.txt` or switched from the **Battery** row in Settings (press 1 to cycle through the drivers; it applies immediately).

While on battery, a warning pops up at 15% ("Battery low") and 5% ("Battery critical"). If the screen is off it doesn't wake it; the warning shows the next time you press the button. Plugging in resets the warnings. While charging (or plugged in and full) the icon turns green and shows a lightning bolt.

- `battery=auto` (default) uses `sysfs` if the kernel reports a battery, otherwise `pisugar` if the server answers, otherwise shows the infinity symbol. Re-checked every 10 s, so a battery that shows up later is picked up.
- `battery=none` always shows a full battery with an infinity symbol. Use it when there is no battery to read, or when the battery module can't report its level (the PiSugar S can't).
- `battery=pisugar` reads the level and charging state from PiSugar's power manager (`pisugar-server`, e.g. for a PiSugar 2 or 3). It must be installed and running. The driver talks to `/tmp/pisugar.sock`, falling back to TCP `127.0.0.1:8423`. A `?` means it could not reach the server.
- `battery=sysfs` reads the standard Linux battery interface (`/sys/class/power_supply`). Works on laptops and with any battery HAT or fuel gauge that has a kernel driver. Peripheral batteries (mice, headsets) are ignored; multiple batteries are combined. A `?` means no battery was found.

### System: updates and start on boot

The **System** row in Settings shows the running version (or `update!`) and opens:

- **Check for updates** / **Install vX.Y.Z**: press 1 to check GitHub now; once a newer release is found, press 1 again to download it, check its SHA-256, replace the program file and restart into it (playback stops for the restart).
- **Auto check: On/Off** (`update_check` in `config.txt`): checks at startup and then every minute. The check is one small request to github.com, not the rate-limited API. When a new version is out, an **Update?** screen asks *Update now* / *Not now*. It waits until the screen is on and you're not typing, and *Not now* (or 4) isn't asked again until the next start. Nothing installs without a press.
- **Start on boot: On/Off**: writes a boot service for the running init system that starts bman from the current folder as your user. Writing into `/etc` needs root: bman runs itself through `sudo` to write the files. With passwordless sudo (the Raspberry Pi OS default) that just works; otherwise the keyboard asks for your sudo password (shown as stars except the last character; a wrong one asks again, 4 cancels). Without sudo, passwordless `doas` is tried. The service always runs as you, not root. If it still can't, an error pops up (*Not allowed to use sudo*, *System files are read-only*, *Unknown init system*, ...). It only writes files, no init commands, so it takes effect at the next boot. Turning it off removes only what bman wrote.

  | Init (detected from PID 1) | Default on | What's written |
  |---|---|---|
  | systemd | Raspberry Pi OS, Debian, Ubuntu, Pop!_OS, Mint, Fedora, Arch, Manjaro, EndeavourOS, SteamOS, openSUSE | `/etc/systemd/system/bman.service` + enable link; also enables lingering so PipeWire (Bluetooth audio) starts without logging in |
  | OpenRC | Gentoo, Alpine, Artix-OpenRC | `/etc/init.d/bman` + `runlevels/default` link |
  | runit | Void, Artix-runit, Devuan-runit | `run` script in `/etc/sv` (Artix: `/etc/runit/sv`) + service link |
  | dinit | Chimera, Artix-dinit | `/etc/dinit.d/bman` + `boot.d` link |
  | sysvinit / BSD-style | Devuan, MX Linux, antiX, Slackware | a marked block in `/etc/rc.local` (Slackware: `/etc/rc.d/rc.local`) |

  s6 (Artix-s6, Obarun) needs its own tools to enable a service, so it shows *Unknown init system*. NixOS has a read-only `/etc`; add bman to your configuration instead.

Update notes:

- Needs internet, and write access to the folder the `bman` binary is in (run it from a folder your user owns, not a root-owned `/usr/bin`).
- Prebuilt releases exist for x86_64, 64-bit Pi (aarch64) and 32-bit Pi (armv7), glibc 2.36 or newer (Raspberry Pi OS Bookworm, Debian 12, Ubuntu 22.10 and newer, any current Arch/Manjaro/Fedora/Void-glibc). musl systems show *No builds for this system*; build from source there.
- A self-built binary updates too: it's replaced by the release build once a newer version comes out.

**Making a release** (for the maintainer): bump `version` in `Cargo.toml`, commit, then `git tag vX.Y.Z && git push origin vX.Y.Z`. The GitHub Actions workflow (`.github/workflows/release.yml`) builds all three targets in a Debian 12 container, attaches `bman-<target>` and `bman-<target>.sha256` to the release and publishes it. The tag must match the `Cargo.toml` version or the build fails, which stops a release that would offer itself as an update forever.

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

- **Bluetooth**: pair devices from Settings > Bluetooth. Talks to BlueZ (`bluetoothd`) directly over D-Bus, so `bluetoothctl` isn't needed. Press 1 on *Scan*, then 1 on a device to pair, trust and connect it (1 on a connected device disconnects). When headphones connect, audio switches to `wireless` by itself and the song carries on there; if they disconnect, playback pauses and audio goes back to `wired`. Trusted devices reconnect on their own.

  Needs `bluetoothd` running plus an audio server with Bluetooth support running as your user: PipeWire with WirePlumber (and its BlueZ/SPA plugin), or PulseAudio with `pulseaudio-module-bluetooth`. Run only one of them; having both PulseAudio and `pipewire-pulse` running breaks Bluetooth audio. If connecting says *No BT audio service*, the audio server isn't running or lacks Bluetooth support.

The Whisplay driver was written from the published pin layout without the vendor's reference code, so the init sequence, backlight polarity (`BACKLIGHT_ACTIVE_LOW`), SPI speed and orientation are best guesses and may need adjusting on real hardware.

## Adding drivers

Displays and batteries are both pluggable:

- **Display**: implement the `Display` trait (see `whisplay.rs`), add the name to `DRIVERS` in `display_driver.rs`, and add a match arm in `create_display`.
- **Battery**: implement `BatterySource` (see `battery.rs`), and add one `(name, constructor)` line to `BATTERY_DRIVERS`. Config and the Settings row pick it up automatically.

## Building and running

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

For a PC test window, set `driver=window` and `use_system_audio=true` in `config.txt`.

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
| `update.rs` | Self-updater (GitHub releases) |
| `autostart.rs` | Start on boot (writes a systemd/OpenRC/runit/dinit service) |
| `library.rs` | Scanning, albums, artists, playlists |
| `art.rs` | Cover art loading |
| `bluetooth.rs` | Bluetooth pairing and connection |
| `config.rs` | `config.txt` loading and saving |
