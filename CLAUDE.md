# CLAUDE.md

Project context for AI assistants working on **bman**, a single-button music player in Rust for a Raspberry Pi Zero 2 W with a PiSugar Whisplay HAT (240x280 ST7789 screen) and a PiSugar 3 battery. It can also run on a PC in a `minifb` window for testing. See `README.md` (quick start) and `DOCS.md` (full user docs; keep it updated when behaviour changes).

## Working notes

- The owner is not a Rust beginner but prefers short, direct answers and working code over explanations. Casual tone is fine.
- **Don't assume code was compiled.** Much of this was written in a sandbox with no Rust toolchain. Run `cargo build --release` and fix errors before building more on top.
- Keep the AI usage notice at the top of `README.md`.
- `music` (symlink) and `config.txt` are gitignored on purpose. Don't commit them.
- Rust edition 2024. Main crates: rodio 0.23 (symphonia-all), symphonia (aiff, pcm), rppal 0.22 (GPIO, PWM, SPI), minifb, font8x8, lofty 0.25, image 0.25 (jpeg/png/bmp), ctrlc, zbus 5, ureq 3 (rustls), sha2, libc (termios for the terminal driver), alsa 0.11 (system volume mixer; same version cpal uses).
- **Portability.** Must work on any Linux distro regardless of init system (only `autostart.rs` knows about init systems; `root.rs`/`power.rs` call `sudo`, `reboot`, `poweroff`, by the owner's choice). No external commands, no distro-specific paths. Runtime deps: libasound + libc only. Prefer pure-Rust crates over C libraries.

## Architecture

| File | Role |
|---|---|
| `main.rs` | `App` state, navigation stack, button handling, idle timeout, background library rescan, main loop |
| `render.rs` | `Framebuffer`, `Display` trait, `GUI` enum, `View`/`Style`, all drawing, colour-scaled dimming |
| `display_driver.rs` | `DRIVERS`, `create_display`, `WindowDisplay` (PC test window; mouse acts as the button) |
| `terminal.rs` | Terminal display driver (`driver=terminal` or `--driver terminal` = terminal only; `--terminal`/`-t` = `Mirror` in display_driver.rs: config driver + terminal, falls back to terminal only if the config driver fails): truecolor `▀` half blocks, box-filter shrink (fractional `SCALES`) to fit, `-`/`+` zoom and `0` fit (zoom past the window crops), diffed cell redraw, raw termios via `libc`, keys map straight to `Button`s, stderr dup2'd to `bman.log` while active |
| `whisplay.rs` | ST7789 driver over SPI0, backlight (software PWM) |
| `battery.rs` | `BatterySource` drivers (`auto`, `none`, `sysfs`, `pisugar`) in the `BATTERY_DRIVERS` registry, `BatteryMonitor` background thread (live-swappable) |
| `buttons.rs` | `PressCounter`, GPIO button thread with wiring auto-detect |
| `audio.rs` | `AudioPlayer`: rodio path (`use_system_audio=true`) or GPIO PWM path |
| `library.rs` | Scanner (singles, albums, artists, playlists), `.m3u` read/write |
| `art.rs` | Cover art loader on a thread, 96x96 |
| `network.rs` | Wi-Fi via NetworkManager over D-Bus (`zbus`): list/scan, connect (password or saved), forget, radio on/off, IP; `wifi=` config lines imported at startup |
| `bluetooth.rs` | BlueZ over D-Bus (`zbus`, pure Rust): device list, scan, pair (own `NoInputNoOutput` agent), connect; background polling emits Connected/Disconnected events |
| `update.rs` | Self-updater: tag from the `github.com/<repo>/releases/latest` redirect (no API, no rate limit; repo from Cargo.toml `repository`), checks every 60 s, downloads `bman-<target>` + `.sha256`, verifies, renames over `current_exe()`, main loop exits and `exec`s the new binary. Releases built by `.github/workflows/release.yml` on `v*` tags (Debian 12 container, glibc 2.36, x86_64/aarch64/armv7 gnu) |
| `autostart.rs` | Start on boot. The one deliberate exception to init-agnostic: detects from `/proc/1/comm` then /run marks: systemd, OpenRC, runit (Void/Artix/Devuan layouts), dinit, sysvinit/Slackware (marked `rc.local` block); writes a service (marker comment) + enable symlink, no init commands; needs root: when not root it re-runs itself via `sudo -n`, then `sudo -S` with the password from the keyboard (`bman --autostart on|off --user --uid --dir`, handled by `autostart::cli()` at the top of `main`); passwordless `doas` fallback. The only place bman runs another program |
| `root.rs` | sudo helper shared by autostart and power: re-runs own binary as `sudo -n` / `sudo -S` (password on stdin) with `--autostart ...`, `--power ...` or `--rfkill ...`, dispatched by `root::cli()` at the top of `main`; reads back `OK msg` / `ERR msg`; `RootError::{NeedsPassword, WrongPassword, Failed}` |
| `rfkill.rs` | Reads `/sys/class/rfkill/*/{type,soft,hard}`; soft blocks lifted via root helper `--rfkill bluetooth|wlan` (writes `soft`=0). Bluetooth: silent `sudo -n` try at startup, prompt when Bluetooth screen opens / Scan; Wi-Fi: only when switching Wi-Fi on (NM's off can be an rfkill block, so never auto) |
| `power.rs` | Restart / shut down: logind then ConsoleKit2 `CanReboot`/`Reboot(false)` over zbus (no root needed in an active session), else root helper runs `reboot`/`poweroff` |
| `mixer.rs` | System volume via ALSA mixer on `default` (prefers `Master`), opened fresh per call; `config.system_volume: Option<u8>` re-applied at startup (retries) and ~2 s after BT connects (applied twice to beat WirePlumber's restore) |
| `config.rs` | `config.txt` load/save (atomic temp-file rename) |

Drivers are pluggable: display (`driver=` key) and battery (`battery=` key). Display: implement the trait, add the name to the list, add a match arm in `create_display`. Battery: implement `BatterySource` and add a `(name, constructor)` entry to `BATTERY_DRIVERS` (first entry is the default; the Settings row cycles through the list).

## Releases

- Bump `version` in Cargo.toml, commit, tag `vX.Y.Z`, push the tag (workflow builds and publishes). Only when the owner says to push/release.
- Owner's versioning: small fixes = patch (0.1.x), bigger updates = minor (0.x.0). Patch never exceeds 9: the release after 0.x.9 is 0.(x+1).0, not 0.x.10 (workflow enforces it). Notes come from commit messages via `.github/release-notes.sh`: patch = commits since previous tag; x.y.0 = everything since previous x.y.0, grouped by version. So write commit messages as user-facing notes (subject + `- ` bullets); a trailing "; release X.Y.Z" in the subject is stripped.

## Design decisions (don't undo without asking)

- **One button.** Press count within 350 ms: 1 select/play/pause, 2 down, 3 up, 4 back, 5 action, hold (800 ms) help. On Now Playing (owner's choice, matches usual players): 2 next, 3 previous (restarts if >3 s in), 5 loop mode. Music can be started from any screen.
- **Audio output.** `use_system_audio=false` means stereo GPIO PWM (hardware PWM, 8 kHz sample rate, 50 kHz carrier, GPIO 12 and 13, via `dtoverlay=pwm-2chan,pin=12,func=4,pin2=13,func2=4`). `true` uses rodio and is for PC testing. The Whisplay's built-in mono audio is deliberately unused. GPIO 12/13 were chosen to avoid the Whisplay's I2S pins.
- **Library naming.** Folders `Artist - Album`, files `Artist - Album - 01 Title.ext`, singles `Artist - Title.ext`. `music/Artist/Album/` also works. Symlinks are followed (with a visited set to avoid loops). The library is rescanned on every screen change.
- **Idle.** Screen sleeps after `screen_timeout` (30-300 s, default 60; 0 or <30 = never, shown as "Never"). Music keeps playing. Waking press also acts on Now Playing if a song is playing; otherwise it only wakes.
- **Brightness.** Hybrid: backlight software PWM on GPIO 22 down to `BACKLIGHT_MIN_PCT` (50), anything dimmer is done by scaling frame colours. Drivers without backlight control (the window) do all dimming in colour. `Display::set_brightness(percent) -> u8` returns the remaining colour-scale percentage.
- **Battery.** Top-right icon. `battery=auto` (default) tries sysfs (`/sys/class/power_supply`), then pisugar, else shows infinity; re-checked every poll. `battery=none` draws a full battery with an infinity sign. `battery=sysfs` reads the kernel battery. `battery=pisugar` reads `pisugar-server` over `/tmp/pisugar.sock` (TCP `127.0.0.1:8423` fallback). A `?` means the server wasn't reachable. The PiSugar **S** cannot report battery level; PiSugar 2/3 can.
- **Updates.** Never auto-install; a found update wakes the screen and opens an Update now / Not now prompt (deferred only while typing or in help); the screen can't sleep on the prompt, on System while an update is offered, or while installing; Settings > System also installs. Release tag must equal the Cargo.toml version (workflow enforces it).
- **Whisplay pins.** SPI0 (MOSI 10, SCLK 11, CE0 8), DC 27, RESET 4, backlight 22, button 17. ST7789 row offset 20, RGB565, 50 MHz SPI.

## Unverified (best guesses, check on hardware)

- Whisplay ST7789 init sequence, `MADCTL`, `ROW_OFFSET`, `SPI_HZ` (PiSugar's `whisplay.py` couldn't be fetched when this was written).
- `BACKLIGHT_ACTIVE_LOW = true` polarity.
- Backlight PWM flicker at the 50% floor.
- PiSugar server reply format (`get battery`, `get battery_charging`, `get battery_power_plugged`) for the PiSugar 3. Written from the PiSugar 2 README.
- PWM audio RC filter values (about 1 kΩ + 33 nF per channel plus a 10 µF DC-blocking cap) are an untested estimate.
- Button wiring auto-detect (don't hold the button at startup).
- Wi-Fi: listing/IP/saved-network reading tested against NetworkManager on a PC; connect, password entry, forget and on/off not tested yet.
- Updater: check against the GitHub API tested (404 = no releases); download/install/restart and the release workflow not run yet.
- Autostart: detection and non-root refusal tested on Void (runit); writing the services as root not tested on any init.
- Bluetooth D-Bus flow tested against BlueZ 5.86 on a PC (list, power, agent, scan); actual headset pair/connect not tested yet.

## Open items

- First build on the Pi and bring-up of the display, then button, audio, Bluetooth, battery.
- Settings screen rows: Volume (submenu: music, system), Accent, Audio, Bright, Sleep, Battery, Network, Bluetooth, System (submenu: update, auto check, start on boot, restart device, shut down, restart bman). Document exactly how each row is edited in DOCS.md once confirmed.
