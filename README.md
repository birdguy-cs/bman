## AI Usage Notice

Yes, this repository uses AI-generated assistance, but with heavy human supervision. I'm mainly using AI because I'm too lazy to write some of this shit myself. I'd much rather spend my time writing games and other projects. The AI's used are Claude and ChatGPT.

The code, decisions, and overall direction are still human-supervised.

# bman

A lightweight music player built to run on the **Raspberry Pi Zero 2 W**, controlled by a **single button**. It plays MP3, WAV, FLAC, OGG and AIFF, and is designed around a small 240x280 screen (the PiSugar Whisplay HAT). You can also run it on a PC in a window for testing, where mouse clicks or the keyboard act as the button (and you can type on the on-screen keyboard screens).

It does Wi-Fi, Bluetooth headphones, playlists, cover art, battery level, YouTube Music streaming, updates itself, and can start on boot.

**Plug it into a computer and it shows up like an iPod or a phone**: a device called *bman* with Music, Videos, Podcasts and Audiobooks folders. Drag your songs, videos, podcasts and audiobooks in, and they're in the library a few seconds later, while the music keeps playing. See [Copying music from a computer](#copying-music-from-a-computer).

**Full documentation: [DOCS.md](DOCS.md)**

## Getting started

1. **Hardware**: a [Raspberry Pi Zero 2 W](https://www.amazon.com/dp/B09LH5SBPS), a [PiSugar Whisplay HAT](https://www.amazon.com/dp/B0FPG8S6K6), a [PiSugar 3](https://www.amazon.com/dp/B0FB3N1YSK) battery and a [3.5 mm jack breakout](https://www.amazon.com/dp/B0H6F3PZGK) for headphones. Wiring and the audio overlay: [Hardware](DOCS.md#hardware).
2. **Install** (Pi or PC, any glibc Linux):

   ```
   curl -fsSL https://github.com/birdguy-cs/bman/releases/latest/download/install.sh | sh
   ```

   (or `wget -qO- .../install.sh | sh`). It downloads the right build for your machine into `~/bman`, checks it, makes `~/bman/music` and `~/bman/videos` (bman makes its other folders itself), and puts a start script at `~/start-bman.sh`. On a PC it also sets up the test window. It also lets bman restart / shut down the device without a password (asks for sudo once). Run it again any time to update; your music and settings are kept. Uninstall with `sh ~/bman/install.sh --uninstall`. Prefer doing it by hand? See [Installing](DOCS.md#installing).
3. **Add music** to `~/bman/music` (see [Your music](#your-music)). On the Pi the easiest way is a USB cable from your computer: see [Copying music from a computer](#copying-music-from-a-computer).
4. **Start it**: `~/start-bman.sh`. Options are passed along, e.g. `~/start-bman.sh --terminal` to also see it in the terminal over SSH (keys 1-5 / arrows as the button, [details](DOCS.md#using-it-over-ssh-terminal-driver)).
5. **Start on boot** (optional): Settings > System > Start on boot.

Updates show up on their own: when a new version is out bman asks *Update now* / *Not now*.

## Controls

There is one button. What it does depends on how many times you press it quickly (within about 0.35 s of each other).

| Presses | Action |
|---|---|
| 1 | Select / play / pause |
| 2 | Move down (on Now Playing: next song; podcasts and audiobooks: forward 30 s) |
| 3 | Move up (on Now Playing: previous song, or restart it if past 3 s; podcasts and audiobooks: back 15 s) |
| 4 | Back |
| 5 | Action (add the highlighted song to the queue or the selected playlist; remove it when you are in a playlist or the queue; on Now Playing: change loop mode: Off, Track, All, Random; podcasts and audiobooks: change speed) |
| Hold (about 0.8 s) | Help screen (what the button does on this screen) |

**Random** loops the queue in random order: every song plays once before any repeats, never the same song twice in a row (unless it's the only one), and 3 presses goes back through what played.

Music can be started from any screen. When the screen is asleep, the first press wakes it; if a song is playing, that press also acts on Now Playing (so 1 press pauses or plays).

## Screens

- **Singles**: songs sitting directly in the music folder.
- **Albums**: folders of songs, showing the artist and track list.
- **Artists**: browse artist, then album, then single.
- **Playlists**: your `.m3u` files. Create, name, rename, add to and remove from them.
- **Videos**: `.mp4` / `.m4v` / `.mov` files from the `videos` folder, with the picture on Now Playing; see [Videos](DOCS.md#videos).
- **Podcasts**: search and subscribe (or list feeds in `podcasts/feeds.txt`); new episodes download over Wi-Fi. See [Podcasts](DOCS.md#podcasts).
- **Audiobooks**: books as folders of chapters or `.m4b` files, a chapter list, and free LibriVox books to download. Podcasts and audiobooks remember where you stopped, play at 0.75x-2x with normal-sounding voices, and 2/3 skip 30 s forward / 15 s back. See [Audiobooks](DOCS.md#audiobooks).
- **Queue**: what is playing and what is next. The first row saves the whole queue as a playlist.
- **USB transfer**: plug it into a computer and it shows up like a phone or music player with Music, Videos, Podcasts and Audiobooks folders to drag files into, while it keeps playing; see [USB transfer](DOCS.md#usb-transfer).
- **Streaming**: YouTube Music search and playback with 144p music videos on Now Playing, plus liked songs: bman's own list, or your account's likes with a cookies file. yt-dlp and Deno are downloaded when you set it up; see [Streaming](DOCS.md#streaming-youtube-music).
- **Now Playing**: cover art (or the music video), title, artist, album, progress and loop mode (the speed for podcasts and audiobooks); a loading bar with the time left while a streamed song downloads.
- **Settings**: volume (music and system), accent colour, audio output, brightness, sleep timer, battery, Wi-Fi, Bluetooth, and System (updates, start on boot, restart / shut down, restart bman, USB transfer).

The top bar on every screen shows the battery and, under the title, what bman is doing: the song playing (or paused), files copying over USB, or an update installing, a restart or a shutdown in progress.

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

## Copying music from a computer

bman on the Pi works like an old iPod: plug it into a computer and drag files onto it. Nothing to install on the computer (except on a Mac, see below).

1. Use a **data** USB cable (some cheap ones only charge) from your computer to the Pi Zero's **USB** port: the inner micro-USB one, not PWR. That one cable can also power the Pi.
2. Start bman (or turn on Start on boot, so it's there whenever the Pi has power).
3. A device called **bman** appears on the computer, with four folders: **Music**, **Videos**, **Podcasts** and **Audiobooks**.
   - **Windows**: in File Explorer under *This PC*, like a phone.
   - **Linux**: in the file manager's sidebar (GNOME, KDE and most desktops; needs MTP support such as `gvfs-mtp` or `kio-extras`, usually already there).
   - **Mac**: macOS can't open these devices on its own; use the free [OpenMTP](https://openmtp.ganeshrvel.com) or Android File Transfer.
4. Drag songs, album folders and videos in (and podcast episodes or audiobooks into their folders, see [Podcasts](DOCS.md#podcasts) and [Audiobooks](DOCS.md#audiobooks)). Name songs like in [Your music](#your-music) so bman sorts them by artist and album. Files dropped outside the folders land in Music (videos in Videos).
5. That's it: they're in the library a few seconds after they finish copying. You can also copy files back off it, rename, make folders and delete. Music keeps playing the whole time.

The first time on a Pi, bman sets the port up and asks for one restart (*USB transfer: restart the Pi to finish*). If your Pi's sudo needs a password, run `sudo ~/bman/bman --usb on` once over SSH (or press **Settings > System > USB transfer** and type it). It's on by default and open to any computer you plug in; turn it off in the same row. Details: [USB transfer](DOCS.md#usb-transfer).

## More

See **[DOCS.md](DOCS.md)** for configuration (`config.txt`), Wi-Fi, Bluetooth, battery drivers, updates and start on boot, hardware wiring, building from source, making releases and adding drivers. Also [USB transfer](DOCS.md#usb-transfer) and [podcasts and audiobooks](DOCS.md#podcasts-and-audiobooks) in full.
