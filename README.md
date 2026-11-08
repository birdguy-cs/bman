## AI Usage Notice

Yes, this repository uses AI-generated assistance, but with heavy human supervision. I'm mainly using AI because I'm too lazy to write some of this shit myself. I'd much rather spend my time writing games and other projects. The AI's used are Claude and ChatGPT.

The code, decisions, and overall direction are still human-supervised.

# bman

A lightweight music player built to run on the **Raspberry Pi Zero 2 W**, controlled by a **single button**. It plays MP3, WAV, FLAC, OGG and AIFF, and is designed around a small 240x280 screen (the PiSugar Whisplay HAT). You can also run it on a PC in a window for testing, where mouse clicks or the keyboard act as the button (and you can type on the on-screen keyboard screens).

It does Wi-Fi, Bluetooth headphones, playlists, cover art, battery level, YouTube Music streaming, updates itself, and can start on boot.

**Full documentation: [DOCS.md](DOCS.md)**

## Getting started

1. **Hardware**: a [Raspberry Pi Zero 2 W](https://www.amazon.com/dp/B09LH5SBPS), a [PiSugar Whisplay HAT](https://www.amazon.com/dp/B0FPG8S6K6), a [PiSugar 3](https://www.amazon.com/dp/B0FB3N1YSK) battery and a [3.5 mm jack breakout](https://www.amazon.com/dp/B0H6F3PZGK) for headphones. Wiring and the audio overlay: [Hardware](DOCS.md#hardware).
2. **Install** (Pi or PC, any glibc Linux):

   ```
   curl -fsSL https://github.com/birdguy-cs/bman/releases/latest/download/install.sh | sh
   ```

   (or `wget -qO- .../install.sh | sh`). It downloads the right build for your machine into `~/bman`, checks it, makes `~/bman/music`, and puts a start script at `~/start-bman.sh`. On a PC it also sets up the test window. It also lets bman restart / shut down the device without a password (asks for sudo once). Run it again any time to update; your music and settings are kept. Uninstall with `sh ~/bman/install.sh --uninstall`. Prefer doing it by hand? See [Installing](DOCS.md#installing).
3. **Add music** to `~/bman/music` (see [Your music](#your-music)).
4. **Start it**: `~/start-bman.sh`. Options are passed along, e.g. `~/start-bman.sh --terminal` to also see it in the terminal over SSH (keys 1-5 / arrows as the button, [details](DOCS.md#using-it-over-ssh-terminal-driver)).
5. **Start on boot** (optional): Settings > System > Start on boot.

Updates show up on their own: when a new version is out bman asks *Update now* / *Not now*.

## Controls

There is one button. What it does depends on how many times you press it quickly (within about 0.35 s of each other).

| Presses | Action |
|---|---|
| 1 | Select / play / pause |
| 2 | Move down (on Now Playing: next song) |
| 3 | Move up (on Now Playing: previous song, or restart it if past 3 s) |
| 4 | Back |
| 5 | Action (add the highlighted song to the queue or the selected playlist; remove it when you are in a playlist or the queue; on Now Playing: change loop mode: Off, Track, All, Random) |
| Hold (about 0.8 s) | Help screen (what the button does on this screen) |

**Random** loops the queue in random order: every song plays once before any repeats, never the same song twice in a row (unless it's the only one), and 3 presses goes back through what played.

Music can be started from any screen. When the screen is asleep, the first press wakes it; if a song is playing, that press also acts on Now Playing (so 1 press pauses or plays).

## Screens

- **Singles**: songs sitting directly in the music folder.
- **Albums**: folders of songs, showing the artist and track list.
- **Artists**: browse artist, then album, then single.
- **Playlists**: your `.m3u` files. Create, name, rename, add to and remove from them.
- **Queue**: what is playing and what is next. The first row saves the whole queue as a playlist.
- **Streaming**: YouTube Music search and playback with 144p music videos on Now Playing, plus liked songs: bman's own list, or your account's likes with a cookies file. yt-dlp and Deno are downloaded when you set it up; see [Streaming](DOCS.md#streaming-youtube-music).
- **Now Playing**: cover art (or the music video), title, artist, album, progress and loop mode; a loading bar with the time left while a streamed song downloads.
- **Settings**: volume (music and system), accent colour, audio output, brightness, sleep timer, battery, Wi-Fi, Bluetooth, and System (updates, start on boot, restart / shut down).

The top bar on every screen shows the battery and, under the title, what bman is doing: the song playing (or paused), or an update installing, a restart or a shutdown in progress.

## Your music

Put songs in the `music` folder (`mp3`, `wav`, `flac`, `ogg`, `aiff`). Name them so bman can tell the artist and album:

```
music/
  Artist - Album/
    Artist - Album - 01 Song Title.flac
    cover.jpg
  Artist - Single Song.mp3
  Song Title by Artist.mp3
  My Playlist.m3u
```

`music/Artist/Album/` folders work too. All the naming forms, cover art and playlist details are in [Music library](DOCS.md#music-library).

## More

See **[DOCS.md](DOCS.md)** for configuration (`config.txt`), Wi-Fi, Bluetooth, battery drivers, updates and start on boot, hardware wiring, building from source, making releases and adding drivers.
