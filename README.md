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
- **Playlists**: your `.m3u` files. Create, add to and remove from them.
- **Queue**: what is playing and what is next. Treated like a playlist.
- **Now Playing**: cover art, title, artist, album, progress and loop mode.
- **Settings**: volume, accent colour, audio output, brightness, sleep timer, battery driver, Bluetooth.

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

### Cover art

Embedded art in the song's tags is used first. If there is none, a `cover.jpg`, `cover.jpeg`, `cover.png` or `cover.bmp` in the song's folder is used. Art is shrunk to 96x96 pixels.

### Playlists

Playlists are standard `.m3u` / `.m3u8` files in the music folder, with one path per line (lines starting with `#` are comments). Paths inside the music folder are saved relative to it. Changes made on the device are written straight back to the file. Entries that cannot be found are usually path mismatches in the `.m3u`.

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
```

### Screen timeout

After the timeout with no input, the screen turns off. Music keeps playing. Press the button to wake it.

### Brightness

On the Whisplay the backlight is dimmed with software PWM down to a flicker-free floor (50%). Anything dimmer is done by scaling the picture's colours. Drivers without a dimmable backlight (the PC window) do all of it with colour. The constants `BACKLIGHT_MIN_PCT` and `BACKLIGHT_PWM_HZ` at the top of `whisplay.rs` are the tuning knobs.

### Battery

A battery icon and percentage sit at the top right of every screen. The driver can be set in `config.txt` or switched from the **Battery** row in Settings (press 1 to cycle through the drivers; it applies immediately).

- `battery=auto` (default) uses `sysfs` if the kernel reports a battery, otherwise `pisugar` if the server answers, otherwise shows the infinity symbol. Re-checked every 10 s, so a battery that shows up later is picked up.
- `battery=none` always shows a full battery with an infinity symbol. Use it when there is no battery to read, or when the battery module can't report its level (the PiSugar S can't).
- `battery=pisugar` reads the level and charging state from PiSugar's power manager (`pisugar-server`, e.g. for a PiSugar 2 or 3). It must be installed and running. The driver talks to `/tmp/pisugar.sock`, falling back to TCP `127.0.0.1:8423`. A `?` means it could not reach the server.
- `battery=sysfs` reads the standard Linux battery interface (`/sys/class/power_supply`). Works on laptops and with any battery HAT or fuel gauge that has a kernel driver. Peripheral batteries (mice, headsets) are ignored; multiple batteries are combined. A `?` means no battery was found.

## Hardware

- **Board**: Raspberry Pi Zero 2 W.
- **Screen**: PiSugar Whisplay HAT (ST7789, 240x280). The built-in mono audio is not used.
- **Button**: GPIO 17. Wiring polarity is detected at startup, so don't hold the button while launching.
- **Audio**: stereo PWM on GPIO 12 and 13 through a low-pass filter. Add this to `/boot/firmware/config.txt`:

  ```
  dtoverlay=pwm-2chan,pin=12,func=4,pin2=13,func2=4
  ```

- **Bluetooth**: pair devices from Settings > Bluetooth (uses `bluetoothctl`). Set audio to `wireless` to play through them.

The Whisplay driver was written from the published pin layout without the vendor's reference code, so the init sequence, backlight polarity (`BACKLIGHT_ACTIVE_LOW`), SPI speed and orientation are best guesses and may need adjusting on real hardware.

## Adding drivers

Displays and batteries are both pluggable:

- **Display**: implement the `Display` trait (see `whisplay.rs`), add the name to `DRIVERS` in `display_driver.rs`, and add a match arm in `create_display`.
- **Battery**: implement `BatterySource` (see `battery.rs`), and add one `(name, constructor)` line to `BATTERY_DRIVERS`. Config and the Settings row pick it up automatically.

## Building and running

```
cargo run --release
```

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
| `library.rs` | Scanning, albums, artists, playlists |
| `art.rs` | Cover art loading |
| `bluetooth.rs` | Bluetooth pairing and connection |
| `config.rs` | `config.txt` loading and saving |
