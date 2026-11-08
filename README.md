## AI Usage Notice

Yes, this repository uses AI-generated assistance, but with heavy human supervision. I'm mainly using AI because I'm too lazy to write some of this shit myself. I'd much rather spend my time writing games and other projects. The AI's used are Claude and ChatGPT.

The code, decisions, and overall direction are still human-supervised.

# bman

A lightweight music player built to run on the **Raspberry Pi Zero 2 W**, controlled by a **single button**. It plays MP3, WAV, FLAC, OGG and AIFF, and is designed around a small 240x280 screen (the PiSugar Whisplay HAT). You can also run it on a PC in a window for testing, where mouse clicks act as the button.

It does Wi-Fi, Bluetooth headphones, playlists, cover art, battery level, updates itself, and can start on boot.

**Full documentation: [DOCS.md](DOCS.md)**

## Getting started

1. **Hardware**: a [Raspberry Pi Zero 2 W](https://www.amazon.com/dp/B09LH5SBPS), a [PiSugar Whisplay HAT](https://www.amazon.com/dp/B0FPG8S6K6), a [PiSugar 3](https://www.amazon.com/dp/B0FB3N1YSK) battery and a [3.5 mm jack breakout](https://www.amazon.com/dp/B0H6F3PZGK) for headphones. Wiring and the audio overlay: [Hardware](DOCS.md#hardware).
2. **Get bman**: download the build for your machine from the [latest release](https://github.com/birdguy-cs/bman/releases/latest) (`bman-aarch64-linux-gnu` for a Pi on 64-bit Raspberry Pi OS, `bman-armv7-linux-gnueabihf` for 32-bit, `bman-x86_64-linux-gnu` for a PC), or [build it yourself](DOCS.md#building).
3. **Set up a folder** your user owns, with the program and your music in it:

   ```
   mkdir ~/bman && cd ~/bman
   mv ~/Downloads/bman-aarch64-linux-gnu bman && chmod +x bman
   mkdir music        # copy your songs in here (or symlink it)
   ./bman
   ```

4. **On a PC**, create `config.txt` next to it with `driver=window` and `use_system_audio=true` to get a test window.
5. **Start on boot** (optional): Settings > System > Start on boot.

Updates show up on their own: when a new version is out bman asks *Update now* / *Not now*.

## Controls

There is one button. What it does depends on how many times you press it quickly (within about 0.35 s of each other).

| Presses | Action |
|---|---|
| 1 | Select / play / pause |
| 2 | Move down |
| 3 | Move up (on the Now Playing screen: change loop mode) |
| 4 | Back |
| 5 | Action (add the highlighted song to the queue or the selected playlist; remove it when you are in a playlist or the queue) |
| Hold (about 0.8 s) | Help screen (what the button does on this screen) |

Music can be started from any screen. When the screen is asleep, the first press wakes it; if a song is playing, that press also acts on Now Playing (so 1 press pauses or plays).

## Screens

- **Singles**: songs sitting directly in the music folder.
- **Albums**: folders of songs, showing the artist and track list.
- **Artists**: browse artist, then album, then single.
- **Playlists**: your `.m3u` files. Create, name, rename, add to and remove from them.
- **Queue**: what is playing and what is next. The first row saves the whole queue as a playlist.
- **Now Playing**: cover art, title, artist, album, progress and loop mode.
- **Settings**: volume, accent colour, audio output, brightness, sleep timer, battery, Wi-Fi, Bluetooth, and System (updates, start on boot).

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
