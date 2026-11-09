#[macro_use]
mod log;
mod art;
mod audio;
mod autostart;
mod battery;
mod bluetooth;
mod buttons;
mod config;
mod display_driver;
mod library;
mod mixer;
mod network;
mod power;
mod render;
mod rfkill;
mod resume;
mod root;
mod sched;
mod streaming;
mod stretch;
mod spoken;
mod video;
mod terminal;
mod update;
mod mtp;
mod usb;
mod whisplay;

use crate::art::*;
use crate::audio::*;
use crate::battery::*;
use crate::bluetooth::*;
use crate::buttons::*;
use crate::config::*;
use crate::display_driver::*;
use crate::library::*;
use crate::network::*;
use crate::power::PowerAction;
use crate::render::*;
use crate::rfkill::Radio;
use crate::root::RootError;
use crate::streaming::{SearchItem, StreamEvent, ToolState, YouTube};
use crate::spoken::{Progress, Spoken, SPEEDS};
use crate::video::Video;
use crate::update::*;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
    Arc,
};
use std::io;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// Folder scanned for music, relative to where the program is run.
const MUSIC_DIR: &str = "music";

/// Minimum time between library rescans (they're asked for on every screen
/// change; files copied in still show up within this long).
const RESCAN_INTERVAL: Duration = Duration::from_secs(5);

/// "Previous" past this point in a song restarts it instead.
const RESTART_THRESHOLD: Duration = Duration::from_secs(3);

/// Before the screen sleeps it dims this long ahead of the timeout...
const SLEEP_DIM_BEFORE: Duration = Duration::from_secs(5);
/// ...taking this long to get down to SLEEP_DIM_PCT of the brightness,
const SLEEP_DIM_RAMP: Duration = Duration::from_millis(800);
const SLEEP_DIM_PCT: u32 = 30;
/// then fades from there to black over the last stretch before it turns off.
const SLEEP_FADE_OUT: Duration = Duration::from_millis(1000);
/// Frame time while fading, so the ramps look smooth.
const FADE_FRAME: Duration = Duration::from_millis(30);

/// How long a toast message stays on screen.
const TOAST_DURATION: Duration = Duration::from_millis(1500);
/// Errors stay up longer so they can be read.
const ERROR_TOAST_DURATION: Duration = Duration::from_secs(4);
/// How long the LED blinks red after an error.
const LED_ERROR_BLINK: Duration = Duration::from_secs(5);
/// Low battery warnings stay up longer.
const BATTERY_TOAST_DURATION: Duration = Duration::from_secs(4);
/// Battery levels (percent) that trigger a warning, highest first.
const BATTERY_WARN_LEVELS: [u8; 2] = [15, 5];
/// The battery must climb this far above a level before it warns again,
/// so a reading wobbling around 15% doesn't keep warning.
const BATTERY_WARN_HYSTERESIS: u8 = 3;

/// Screens refer to albums, artists and playlists by index. After a rescan the
/// indexes can shift, so find each one again in the new library. Returns None
/// if the thing no longer exists (its screen is dropped).
fn remap_gui(gui: GUI, old: &Library, new: &Library) -> Option<GUI> {
    Some(match gui {
        GUI::Singles { selected } => GUI::Singles {
            selected: selected.min(new.singles.len().saturating_sub(1)),
        },
        GUI::Videos { selected } => GUI::Videos {
            selected: selected.min(new.videos.len().saturating_sub(1)),
        },
        GUI::Albums { selected } => GUI::Albums {
            selected: selected.min(new.albums.len().saturating_sub(1)),
        },
        GUI::Artists { selected } => GUI::Artists {
            selected: selected.min(new.artists.len().saturating_sub(1)),
        },
        // Row 0 is "+ New playlist", so there is one more row than playlists.
        GUI::Playlists { selected } => GUI::Playlists {
            selected: selected.min(new.playlists.len()),
        },
        GUI::Album { album, selected } => {
            let label = old.albums.get(album)?.label();
            let album = new.albums.iter().position(|a| a.label() == label)?;
            GUI::Album {
                album,
                selected: selected.min(new.albums[album].songs.len().saturating_sub(1)),
            }
        }
        GUI::Artist { artist, selected } => {
            let name = &old.artists.get(artist)?.name;
            let artist = new.artists.iter().position(|a| &a.name == name)?;
            let a = &new.artists[artist];
            GUI::Artist {
                artist,
                selected: selected.min((a.albums.len() + a.singles.len()).saturating_sub(1)),
            }
        }
        GUI::Playlist { playlist, selected } => {
            let path = &old.playlists.get(playlist)?.path;
            let playlist = new.playlists.iter().position(|p| &p.path == path)?;
            GUI::Playlist {
                playlist,
                // Row 0 is "Rename playlist", so rows = songs + 1.
                selected: selected.min(new.playlists[playlist].songs.len()),
            }
        }
        GUI::Show { show, selected } => {
            let dir = &old.shows.get(show)?.dir;
            let show = new.shows.iter().position(|s| &s.dir == dir)?;
            GUI::Show { show, selected: selected.min(new.shows[show].episodes.len().saturating_sub(1)) }
        }
        GUI::Book { book, selected } => {
            let path = &old.books.get(book)?.path;
            let book = new.books.iter().position(|b| &b.path == path)?;
            GUI::Book { book, selected }
        }
        GUI::Podcasts { selected } => GUI::Podcasts { selected: selected.min(new.shows.len() + 1) },
        GUI::Audiobooks { selected } => GUI::Audiobooks { selected: selected.min(new.books.len()) },
        // Menu, queue, now playing, settings and bluetooth don't depend on the library.
        other => other,
    })
}

/// What the on-screen keyboard is being used for.
#[derive(Clone)]
enum InputPurpose {
    WifiPassword { ssid: String },
    /// Identified by file path, which survives rescans re-sorting playlists.
    PlaylistName { path: PathBuf, name: String },
    /// sudo password for a job that needs root.
    SudoPassword { job: RootJob },
    /// YouTube Music search.
    /// YouTube Music search: songs, or music videos.
    StreamSearch { videos: bool },
    /// Podcast directory search.
    PodcastSearch,
    /// LibriVox search.
    BookSearch,
}

/// Something done as root (maybe through sudo) on a background thread.
#[derive(Clone, Copy)]
enum RootJob {
    /// Start on boot on/off.
    Boot(bool),
    Power(PowerAction),
    /// Unblock a radio. `prompt`: may ask for the sudo password (false for
    /// the silent try at startup).
    Unblock { radio: Radio, prompt: bool },
    /// Set up USB transfer (configfs gadget). `prompt` as for Unblock.
    Usb { prompt: bool },
}

/// System screen rows.
const SYSTEM_ROWS: usize = 7;
const ROW_RESTART: usize = 3;
const ROW_SHUTDOWN: usize = 4;
const ROW_RESTART_APP: usize = 5;
const ROW_USB: usize = 6;
/// Restart / shut down need a second press within this time.
const CONFIRM_WINDOW: Duration = Duration::from_secs(3);

/// What a button press does to the screen stack.
enum Nav {
    Stay,
    Replace(GUI),
    Push(GUI),
}

struct App {
    library: Library,
    player: AudioPlayer,
    bluetooth: Bluetooth,
    battery: BatteryMonitor,
    /// Live settings. Volume, accent and output are edited from the settings screen.
    config: Config,

    /// Songs to play through, in order. Selecting a song in a list replaces
    /// the queue with that list. 5 presses on a song adds it to the queue
    /// (or to the selected playlist).
    queue: Vec<Song>,
    /// Index into `queue` of the song being played.
    current: Option<usize>,
    loop_mode: LoopMode,
    /// Playlist that 5 presses add to. None = the queue.
    target: Option<usize>,

    /// Screen history. Never empty; Back pops (except the last entry).
    nav: Vec<GUI>,
    /// Restarted on every button press and track change so long text
    /// starts scrolling from the beginning.
    anim_start: Instant,

    art: Option<Art>,
    art_rx: Option<Receiver<Option<Art>>>,
    toast: Option<(String, Instant, Duration)>,
    /// Lowest battery warning level already shown (reset when charging).
    battery_warned: Option<u8>,
    /// A battery warning that came up while the screen was off; shown on wake.
    battery_warning_pending: Option<String>,
    /// Help overlay, opened by a long press.
    help: bool,

    /// The screen is off after a while without input. Music keeps playing.
    idle: bool,
    last_input: Instant,

    /// Background scan of the music folder (started on every screen change),
    /// with the playlist-edit count it started at.
    scan_rx: Option<(Receiver<io::Result<Library>>, u32)>,
    /// Another scan was asked for while one was running (or too soon).
    rescan_pending: bool,
    /// When the last scan started, to space them out.
    last_scan: Option<Instant>,
    /// Counts our own playlist changes, so a scan that read the files before
    /// a change was saved isn't applied.
    library_edits: u32,
    last_scan_error: Option<String>,
    /// When the last error was shown (the LED blinks red for a while).
    error_at: Option<Instant>,
    /// Restart / shut down went through: the LED stays red until power goes.
    powering_off: bool,
    /// Clock for LED blinking.
    started: Instant,

    network: Network,
    /// What the keyboard screen is for, and the text typed so far.
    input_purpose: Option<InputPurpose>,
    input_text: String,

    updater: Updater,
    /// A newer version was found; show the "Update?" prompt when the screen
    /// is on and not in the middle of typing.
    update_prompt: Option<String>,
    /// Version the user said "Not now" to (not asked again this run).
    update_dismissed: Option<String>,
    /// Init system found for "Start on boot", and whether it's enabled.
    init: Option<autostart::Init>,
    boot_enabled: bool,
    /// A root job running in the background.
    root_job: Option<(Receiver<Result<String, RootError>>, RootJob)>,
    /// System row pressed once (restart / shut down), waiting for the second press.
    power_armed: Option<(usize, Instant)>,
    /// Last read system volume (None = no usable mixer), and when.
    system_volume_now: Option<u8>,
    system_volume_read_at: Instant,
    /// Re-apply the saved system volume at this time (`tries` left if the
    /// mixer isn't there yet, `repeats` more times after it worked, so it
    /// wins over the sound server restoring a device's old volume).
    system_volume_apply: Option<(Instant, u8, u8)>,
    /// An update was installed while a song was playing: restart into it at
    /// the next song change (natural end, skip or picking another song).
    restart_after_song: bool,
    /// An update was installed: leave the main loop at this time and restart
    /// into the new binary (the delay lets the toast show).
    restart_at: Option<Instant>,

    /// Podcasts and audiobooks: downloads and searches, saved positions.
    spoken: Spoken,
    progress: Progress,
    /// When the playing podcast / audiobook position was last saved.
    spoken_saved: Instant,
    /// The podcast / audiobook file the player has open (the queue may
    /// already hold something else when it's replaced).
    spoken_playing: Option<PathBuf>,
    /// An episode picked before it was downloaded: plays when it's there.
    spoken_wait: Option<Song>,
    /// Chapters of the book on the Book screen (read once when it opens).
    book_chapters: Vec<spoken::Chapter>,
    /// 5 pressed once on a show (unsubscribe needs a second press).
    unsubscribe_armed: Option<(usize, Instant)>,
    /// When podcast feeds were last refreshed (None = not yet).
    feeds_refreshed: Option<Instant>,

    /// USB transfer (the port as an MTP device), once started.
    usb: Option<usb::Usb>,
    usb_status: mtp::Status,
    /// USB setup is waiting for another root job to finish.
    usb_setup_pending: Option<bool>,
    /// Last USB setup message worth showing on System (e.g. restart needed).
    usb_note: Option<String>,
    /// Last look at whether the boot step got the port ready.
    usb_checked: Instant,

    youtube: YouTube,
    /// The current song is streamed and still downloading: its id, and where
    /// to start once it's there.
    stream_wait: Option<(String, Duration)>,
    /// 5 on a streamed song likes it instead of adding it to the queue.
    stream_to_liked: bool,
    /// The playing song's video (streamed music video, or a file from the
    /// videos folder): the queue path it belongs to, and the player.
    video: Option<(PathBuf, Video)>,

    /// Random loop: queue indices not played yet this round, the song
    /// picked to play next, and the songs played before (for "previous").
    shuffle_bag: Vec<usize>,
    shuffle_next: Option<usize>,
    shuffle_history: Vec<usize>,
    /// The next round is the first: the playing song counts as played.
    shuffle_fresh: bool,
}

/// YouTube Music screen rows.
const YT_ROW_SEARCH: usize = 0;
const YT_ROW_SEARCH_VIDEOS: usize = 1;
const YT_ROW_RESULTS: usize = 2;
const YT_ROW_LIKED: usize = 3;
const YT_ROW_ACCOUNT: usize = 4;
const YT_ROW_VIDEOS: usize = 5;
const YT_ROW_SETUP: usize = 6;
const YT_ROWS: usize = 7;

/// Choices for the screen timeout setting (seconds): 30 s up to 5 min.
const TIMEOUTS: [u32; 8] = [30, 45, 60, 90, 120, 180, 240, 300];

/// The next longer/shorter preset. Works for any value, e.g. a hand-edited 75.
/// Below 30 s is 0: never sleep.
fn step_timeout(current: u32, up: bool) -> u32 {
    if up {
        TIMEOUTS.iter().copied().find(|&t| t > current).unwrap_or(300)
    } else {
        TIMEOUTS.iter().rev().copied().find(|&t| t < current).unwrap_or(0)
    }
}

/// Move a selection down/up, wrapping around at the ends.
fn step(selected: usize, count: usize, down: bool) -> usize {
    if count == 0 {
        0
    } else if down {
        (selected + 1) % count
    } else {
        (selected + count - 1) % count
    }
}

/// Down/Up on a list: returns the Nav that moves the selection.
fn move_selection(
    button: Button,
    selected: usize,
    count: usize,
    make: impl Fn(usize) -> GUI,
) -> Option<Nav> {
    match button {
        Button::Down => Some(Nav::Replace(make(step(selected, count, true)))),
        Button::Up => Some(Nav::Replace(make(step(selected, count, false)))),
        _ => None,
    }
}

impl App {
    fn new(library: Library, player: AudioPlayer, battery: BatteryMonitor, config: Config) -> Self {
        let stream_video = config.stream_video;
        let network = Network::new();
        network.import(config.wifi.clone());
        let updater = Updater::new(config.update_check);
        Self {
            library,
            player,
            bluetooth: Bluetooth::new(),
            battery,
            config,
            queue: Vec::new(),
            current: None,
            loop_mode: LoopMode::Off,
            target: None,
            nav: vec![GUI::Menu { selected: 0 }],
            anim_start: Instant::now(),
            art: None,
            art_rx: None,
            toast: None,
            battery_warned: None,
            battery_warning_pending: None,
            help: false,
            idle: false,
            last_input: Instant::now(),
            scan_rx: None,
            rescan_pending: false,
            last_scan: None,
            library_edits: 0,
            last_scan_error: None,
            error_at: None,
            powering_off: false,
            started: Instant::now(),
            network,
            input_purpose: None,
            input_text: String::new(),
            updater,
            update_prompt: None,
            update_dismissed: None,
            init: autostart::detect(),
            boot_enabled: false,
            root_job: None,
            power_armed: None,
            system_volume_now: None,
            system_volume_read_at: Instant::now() - Duration::from_secs(60),
            system_volume_apply: None,
            restart_after_song: false,
            restart_at: None,
            spoken: Spoken::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
            progress: Progress::load(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
            spoken_saved: Instant::now(),
            spoken_playing: None,
            spoken_wait: None,
            book_chapters: Vec::new(),
            unsubscribe_armed: None,
            feeds_refreshed: None,
            usb: None,
            usb_status: mtp::Status::default(),
            usb_setup_pending: None,
            usb_note: None,
            usb_checked: Instant::now(),
            youtube: {
                let yt = YouTube::new();
                yt.set_video(stream_video);
                yt
            },
            stream_wait: None,
            stream_to_liked: false,
            video: None,
            shuffle_bag: Vec::new(),
            shuffle_next: None,
            shuffle_history: Vec::new(),
            shuffle_fresh: true,
        }
    }

    fn gui(&self) -> &GUI {
        self.nav.last().expect("nav stack is never empty")
    }

    fn reset_anim(&mut self) {
        self.anim_start = Instant::now();
    }

    fn anim_ms(&self) -> u64 {
        self.anim_start.elapsed().as_millis() as u64
    }

    fn volume_pct(&self) -> u8 {
        (self.config.volume * 100.0).round().clamp(0.0, 100.0) as u8
    }

    /// Wireless output and the PC test flag both use the system audio device.
    fn uses_system_audio(&self) -> bool {
        self.config.use_system_audio || self.config.output == Output::Wireless
    }

    fn show_toast(&mut self, message: impl Into<String>) {
        self.show_toast_for(message, TOAST_DURATION);
    }

    fn show_toast_for(&mut self, message: impl Into<String>, duration: Duration) {
        self.toast = Some((message.into(), Instant::now(), duration));
    }

    /// An error toast; also blinks the LED red.
    fn show_error(&mut self, message: impl Into<String>) {
        self.error_at = Some(Instant::now());
        self.show_toast_for(message, ERROR_TOAST_DURATION);
    }

    /// Status LED colour right now (blinking is by time, so call it often).
    fn led(&self) -> Led {
        let ms = self.started.elapsed().as_millis();
        let powering = self.powering_off
            || matches!(self.root_job, Some((_, RootJob::Power(_))));
        if powering {
            return Led::Red;
        }
        if self.error_at.is_some_and(|t| t.elapsed() < LED_ERROR_BLINK) {
            return if ms / 250 % 2 == 0 { Led::Red } else { Led::Off };
        }
        if matches!(self.updater.state(), UpdateState::Installing) {
            return if ms / 500 % 2 == 0 { Led::Green } else { Led::Blue };
        }
        let playing = (self.current.is_some() && !self.player.is_paused())
            || self.stream_wait.is_some();
        if playing { Led::Blue } else { Led::Green }
    }

    fn active_toast(&self) -> Option<&str> {
        self.toast
            .as_ref()
            .filter(|(_, shown, duration)| shown.elapsed() < *duration)
            .map(|(message, _, _)| message.as_str())
    }

    /// Warn once each time the battery drops past a level in
    /// BATTERY_WARN_LEVELS. Plugging in resets it. If the screen is off the
    /// warning waits until it wakes (the screen isn't turned on for it).
    fn check_battery(&mut self) {
        let Battery::Level { percent, charging } = self.battery.get() else {
            return;
        };

        if charging {
            self.battery_warned = None;
            self.battery_warning_pending = None;
            return;
        }

        // Climbed back well above the warned level (e.g. a bad reading).
        if let Some(level) = self.battery_warned {
            if percent > level + BATTERY_WARN_HYSTERESIS {
                self.battery_warned = BATTERY_WARN_LEVELS
                    .iter()
                    .copied()
                    .filter(|&l| percent <= l + BATTERY_WARN_HYSTERESIS)
                    .min();
            }
        }

        let crossed = BATTERY_WARN_LEVELS
            .iter()
            .copied()
            .filter(|&l| percent <= l && self.battery_warned.is_none_or(|w| l < w))
            .min();

        if let Some(level) = crossed {
            self.battery_warned = Some(level);
            let message = if level == *BATTERY_WARN_LEVELS.last().unwrap() {
                format!("Battery critical: {percent}%")
            } else {
                format!("Battery low: {percent}%")
            };
            warn!("{message}");
            self.battery_warning_pending = Some(message);
        }

        if !self.idle {
            if let Some(message) = self.battery_warning_pending.take() {
                self.show_toast_for(message, BATTERY_TOAST_DURATION);
            }
        }
    }

    fn now_playing(&self) -> Option<NowPlayingInfo> {
        let song = self.queue.get(self.current?)?;

        Some(NowPlayingInfo {
            title: song.title.clone(),
            artist: song
                .artist
                .clone()
                .unwrap_or_else(|| "Unknown Artist".to_string()),
            album: song.album.clone(),
            length: self.player.length_secs(),
            time: self.player.position_secs(),
            paused: self.player.is_paused(),
            loading: self.stream_wait.is_some(),
            download: self
                .stream_wait
                .as_ref()
                .and_then(|(id, _)| self.youtube.progress(id)),
            loop_mode: self.loop_mode,
            speed: spoken::is_spoken(&song.path).then_some(self.config.spoken_speed),
        })
    }

    fn playlist_name(&self, index: usize) -> String {
        self.library
            .playlists
            .get(index)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    // --- playback -----------------------------------------------------

    fn start_queue(&mut self, songs: Vec<Song>, index: usize) {
        self.queue = songs;
        self.reset_shuffle();
        self.play_index(index);
    }

    /// Add a song to the end of the queue.
    fn enqueue(&mut self, song: Song) {
        self.queue.push(song);
        if !self.shuffle_bag.is_empty() {
            self.shuffle_bag.push(self.queue.len() - 1); // part of this round
        }
        self.plan_shuffle();
        self.prefetch_next();
    }

    // --- random loop ----------------------------------------------------

    /// Forget the round (the queue's indices changed).
    fn reset_shuffle(&mut self) {
        self.shuffle_bag.clear();
        self.shuffle_fresh = true;
        self.shuffle_next = None;
        self.shuffle_history.clear();
    }

    /// In Random mode, pick the song after the current one (if it isn't
    /// picked yet). Every song plays once before a new round starts, and a
    /// new round never starts with the song that just played.
    fn plan_shuffle(&mut self) {
        if self.loop_mode != LoopMode::Random {
            self.shuffle_next = None;
            return;
        }
        let len = self.queue.len();
        let current = self.current;
        self.shuffle_bag.retain(|&j| j < len && Some(j) != current);
        if self.shuffle_next.is_some_and(|n| n < len && Some(n) != current) {
            return;
        }
        self.shuffle_next = shuffle_pick(&mut self.shuffle_bag, len, current, self.shuffle_fresh);
        self.shuffle_fresh = false;
    }

    /// The song that plays after the current one (what gets fetched ahead).
    fn upcoming(&self) -> Option<usize> {
        let i = self.current?;
        if self.loop_mode == LoopMode::Random {
            return self.shuffle_next;
        }
        Some((i + 1) % self.queue.len().max(1))
    }

    /// Random mode moving forward: remember where we were, play the pick.
    fn play_shuffled(&mut self) {
        self.plan_shuffle();
        let Some(next) = self.shuffle_next else { return };
        if let Some(i) = self.current.filter(|&i| i != next) {
            self.shuffle_history.push(i);
            if self.shuffle_history.len() > 200 {
                self.shuffle_history.remove(0);
            }
        }
        self.play_index(next);
    }

    fn play_index(&mut self, index: usize) {
        self.play_index_from(index, Duration::ZERO);
    }

    fn play_index_from(&mut self, index: usize, start: Duration) {
        // An update is waiting for the song to change: restart into it now
        // and let the new version start this song.
        if self.restart_after_song {
            self.restart_with_resume(Some(index), Duration::ZERO, false, "update");
            return;
        }
        // Leaving a podcast / audiobook: remember where it was.
        self.save_spoken_position();
        let Some(song) = self.queue.get(index) else {
            self.current = None;
            return;
        };
        let path = song.path.clone();
        self.spoken_wait = None;

        self.current = Some(index);
        self.plan_shuffle();
        self.reset_anim();
        self.art = None;
        self.art_rx = None;
        self.stream_wait = None;
        self.video = None;
        self.clean_streams();

        // A streamed song that isn't downloaded yet: wait for it (update()
        // starts it when it's there).
        if let Some(id) = self.youtube.id_of(&path).filter(|_| !path.is_file()) {
            self.player.stop();
            self.youtube.fetch(&id, true);
            self.stream_wait = Some((id, start));
            return;
        }

        self.spoken_playing = spoken::is_spoken(&path).then(|| path.clone());
        if spoken::is_spoken(&path) {
            info!("Playing {} from {} at {}x", path.display(), clock(start.as_secs()), self.config.spoken_speed);
            self.player.play_from_speed(path.clone(), start, self.config.spoken_speed);
            self.progress.set(&path, start.as_secs(), 0);
            self.spoken_saved = Instant::now();
        } else {
            self.player.play_from(path.clone(), start);
        }
        if let Some(id) = self.youtube.id_of(&path) {
            self.open_video(&id, start);
        } else if video::is_video_file(&path) {
            self.video = Some((path.clone(), Video::open(&path, start)));
        }

        // Load cover art in the background; the circle shows until it's ready.
        self.art_rx = Some(spawn_art_loader(path));
        self.prefetch_next();
    }

    // --- streaming ----------------------------------------------------

    /// Ids of streamed songs that are playing or next (their files are kept).
    fn wanted_streams(&self) -> Vec<String> {
        let Some(i) = self.current else { return Vec::new() };
        [Some(i), self.upcoming()]
            .iter()
            .flatten()
            .filter_map(|&j| self.queue.get(j))
            .filter_map(|song| self.youtube.id_of(&song.path))
            .collect()
    }

    /// Delete downloaded songs that aren't playing or next.
    fn clean_streams(&self) {
        self.youtube.clean(&self.wanted_streams());
    }

    /// Start downloading the next song if it's streamed.
    fn prefetch_next(&self) {
        if let Some(next) = self.upcoming().and_then(|n| self.queue.get(n)) {
            if let Some(id) = self.youtube.id_of(&next.path) {
                self.youtube.fetch(&id, false);
            }
        }
    }

    fn handle_streams(&mut self) {
        for event in self.youtube.take_events() {
            match event {
                StreamEvent::Ready { id, meta } => {
                    let path = self.youtube.song_path(&id);
                    if let Some(meta) = meta {
                        for song in self.queue.iter_mut().filter(|s| s.path == path) {
                            song.title = meta.title.clone();
                            song.artist = meta.artist.clone();
                            if let Some(album) = &meta.album {
                                song.album = album.clone();
                            }
                        }
                    }
                    if self.stream_wait.as_ref().is_some_and(|(w, _)| *w == id) {
                        let start = self.stream_wait.take().map(|(_, s)| s).unwrap_or_default();
                        if let Some(i) = self.current {
                            self.play_index_from(i, start);
                        }
                    }
                }
                StreamEvent::VideoReady { id } => {
                    if self.video.is_none() && self.stream_wait.is_none() && self.current_stream().as_deref() == Some(id.as_str()) {
                        let at = self.player.position();
                        self.open_video(&id, at);
                    }
                }
                StreamEvent::Failed { id, error } => {
                    // Failures of songs fetched ahead (or stopped for another) don't matter.
                    if self.stream_wait.as_ref().is_some_and(|(w, _)| *w == id) {
                        self.stream_wait = None;
                        self.current = None;
                        self.art = None;
                        self.show_error(error);
                    }
                }
            }
        }
    }

    /// The id of the playing song if it's streamed.
    fn current_stream(&self) -> Option<String> {
        self.youtube.id_of(&self.queue.get(self.current?)?.path)
    }

    /// Show a streamed song's video from `at`, if Videos is on and it's downloaded.
    fn open_video(&mut self, id: &str, at: Duration) {
        let path = self.youtube.video_path(id);
        if self.config.stream_video && path.is_file() {
            self.video = Some((self.youtube.song_path(id), Video::open(&path, at)));
        }
    }

    /// The video frame to show now: only on Now Playing, for the playing song.
    fn video_frame(&mut self) -> Option<Vec<u32>> {
        if !matches!(self.gui(), GUI::NowPlaying) || self.help {
            return None;
        }
        let song = self.queue.get(self.current?)?.path.clone();
        let pos = self.player.position();
        let (vid, video) = self.video.as_mut()?;
        if *vid != song {
            return None;
        }
        video.frame(pos).map(|f| f.to_vec())
    }

    /// (status line, rows) for the YouTube Music screen.
    fn yt_rows(&self) -> (String, Vec<String>) {
        let (status, setup) = match self.youtube.state() {
            ToolState::Missing => ("Not set up".to_string(), "Set up"),
            ToolState::Working(what) => (what, "Working..."),
            ToolState::Ready => ("Ready".to_string(), "Update yt-dlp, Deno"),
            ToolState::Broken(e) => (e, "Set up again"),
            ToolState::Unsupported => ("Needs a 64-bit system".to_string(), "Not available"),
        };
        let liked = format!(
            "{}Liked songs ({})",
            if self.stream_to_liked { "* " } else { "" },
            self.youtube.liked().len()
        );
        let videos = format!("Videos: {}", if self.config.stream_video { "On" } else { "Off" });
        (
            status,
            vec![
                "Search".into(),
                "Search videos".into(),
                "Last results".into(),
                liked,
                "YouTube likes".into(),
                videos,
                setup.into(),
            ],
        )
    }

    /// The songs of a stream list.
    fn stream_list(&self, list: StreamList) -> Vec<SearchItem> {
        match list {
            StreamList::Results => self.youtube.results(),
            StreamList::Liked => self.youtube.liked(),
            StreamList::Account => self.youtube.account_likes(),
        }
    }

    /// (title, status line, songs) for a stream list screen.
    fn stream_list_view(&self, list: StreamList) -> (&'static str, String, Vec<String>) {
        let items: Vec<String> = self.stream_list(list).into_iter().map(|r| r.title).collect();
        match list {
            StreamList::Results => ("Results", self.youtube.search_status(), items),
            StreamList::Liked => ("Liked songs", "5: unlike".into(), items),
            StreamList::Account => ("YouTube likes", self.youtube.account_status(), items),
        }
    }

    /// Skip to the next track in the queue (wraps to the start).
    fn skip_next(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.loop_mode == LoopMode::Random && self.current.is_some() {
            self.play_shuffled();
            return;
        }
        let next = match self.current {
            Some(i) => (i + 1) % self.queue.len(),
            None => 0,
        };
        self.play_index(next);
    }

    /// Back a track, like most players: more than a few seconds into the
    /// song restarts it; near its start goes to the previous song (wraps).
    fn skip_previous(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let previous = match self.current {
            Some(i) if self.player.position() > RESTART_THRESHOLD => i,
            // Random: back through the songs that played (or restart).
            Some(i) if self.loop_mode == LoopMode::Random => self
                .shuffle_history
                .pop()
                .filter(|&p| p < self.queue.len())
                .unwrap_or(i),
            Some(i) => (i + self.queue.len() - 1) % self.queue.len(),
            None => 0,
        };
        self.play_index(previous);
    }

    /// The current track ended on its own: apply the loop mode.
    fn on_track_end(&mut self) {
        let Some(i) = self.current else {
            return;
        };
        let len = self.queue.len();

        // Podcasts and audiobooks: finished; the next chapter follows (no
        // loop modes).
        if let Some(path) = self.queue.get(i).map(|s| s.path.clone()).filter(|p| spoken::is_spoken(p)) {
            self.spoken_playing = None; // ended: nothing left to save
            self.progress.set_done(&path, true);
            self.progress.save();
            if i + 1 < len {
                self.play_index(i + 1);
            } else {
                self.current = None;
                self.art = None;
            }
            return;
        }

        if self.loop_mode == LoopMode::Random {
            self.play_shuffled();
            return;
        }
        let next = match self.loop_mode {
            LoopMode::Track => Some(i),
            LoopMode::Random => unreachable!(),
            LoopMode::Queue => Some((i + 1) % len.max(1)),
            LoopMode::Off if i + 1 < len => Some(i + 1),
            LoopMode::Off => None,
        };

        match next {
            Some(n) => self.play_index(n),
            None => {
                self.current = None;
                self.art = None;
                // End of the queue: nothing to wait for any more.
                if self.restart_after_song {
                    self.restart_with_resume(None, Duration::ZERO, false, "update");
                }
            }
        }
    }

    // --- restarting with the queue ------------------------------------

    /// Save the queue (to play `index` from `start`) and restart shortly.
    fn restart_with_resume(&mut self, index: Option<usize>, start: Duration, paused: bool, reason: &str) {
        resume::save(&resume::Resume {
            reason: reason.to_string(),
            songs: self.queue.iter().map(|s| s.path.clone()).collect(),
            index,
            position_ms: start.as_millis() as u64,
            paused,
            loop_mode: self.loop_mode.label().to_string(),
        });
        self.restart_after_song = false;
        self.player.stop();
        self.restart_at = Some(Instant::now() + Duration::from_millis(800));
    }

    /// Restart right now, carrying on the current song from where it is.
    fn restart_now(&mut self, reason: &str) {
        let position = self.player.position();
        let paused = self.player.is_paused();
        self.restart_with_resume(self.current, position, paused, reason);
    }

    /// At startup: pick up the queue saved before a restart, if any.
    fn resume_after_restart(&mut self) {
        let Some(saved) = resume::take() else { return };
        self.queue = saved
            .songs
            .iter()
            .filter_map(|path| {
                self.library.song_by_path(path).or_else(|| {
                    // Not in the library any more but still on disk.
                    if let Some(id) = self.youtube.id_of(path) {
                    // Streamed: downloads again; details come back with it.
                    return Some(self.youtube.song(&SearchItem { title: id.clone(), id }));
                }
                path.exists().then(|| Song {
                        title: path.file_stem().unwrap_or_default().to_string_lossy().to_string(),
                        track: None,
                        path: path.clone(),
                        artist: None,
                        album: String::new(),
                    })
                })
            })
            .collect();
        self.loop_mode = LoopMode::from_label(&saved.loop_mode);
        if let Some(index) = saved.index.filter(|&i| i < self.queue.len()) {
            self.play_index_from(index, Duration::from_millis(saved.position_ms));
            if saved.paused {
                self.player.toggle_pause();
            }
            self.nav = vec![GUI::Menu { selected: 0 }, GUI::NowPlaying];
        }
        info!("Resumed after {}: {} songs in the queue", saved.reason, self.queue.len());
        if saved.reason == "update" {
            self.show_toast(format!("Updated to v{VERSION}"));
        }
    }

    fn remove_from_queue(&mut self, index: usize) {
        if index >= self.queue.len() {
            return;
        }
        self.queue.remove(index);
        self.reset_shuffle();

        match self.current {
            // Removed the playing song: the next one slides into its slot.
            Some(c) if c == index => {
                if index < self.queue.len() {
                    self.play_index(index);
                } else if matches!(self.loop_mode, LoopMode::Queue | LoopMode::Random) && !self.queue.is_empty() {
                    self.play_index(0);
                } else {
                    self.player.stop();
                    self.current = None;
                    self.art = None;
                }
            }
            Some(c) if c > index => self.current = Some(c - 1),
            _ => {}
        }

        self.show_toast("Removed from queue");
    }

    /// Save the whole queue as a new playlist, then ask for its name.
    fn save_queue_as_playlist(&mut self) -> Nav {
        if self.queue.is_empty() {
            self.show_toast("Queue is empty");
            return Nav::Stay;
        }
        // Streamed songs are only downloaded while they play.
        let songs: Vec<Song> = self
            .queue
            .iter()
            .filter(|s| self.youtube.id_of(&s.path).is_none())
            .cloned()
            .collect();
        if songs.is_empty() {
            self.show_toast("No local songs to save");
            return Nav::Stay;
        }
        match self.library.create_playlist_with(songs) {
            Ok(index) => {
                self.library_edits += 1;
                let p = &self.library.playlists[index];
                let purpose = InputPurpose::PlaylistName {
                    path: p.path.clone(),
                    name: p.name.clone(),
                };
                Nav::Push(self.open_keyboard(purpose))
            }
            Err(e) => {
                error!("Couldn't create playlist: {e}");
                self.show_toast("Couldn't create playlist");
                Nav::Stay
            }
        }
    }

    /// 5 presses on a song: add it to the selected playlist, or the queue.
    fn add_to_target(&mut self, song: Song) {
        match self.target {
            None => {
                self.enqueue(song);
                self.show_toast("Added to queue");
            }
            Some(p) => match self.library.add_to_playlist(p, song) {
                Ok(true) => {
                    self.library_edits += 1;
                    let name = self.playlist_name(p);
                    self.show_toast(format!("Added to {name}"));
                }
                Ok(false) => self.show_toast("Already in playlist"),
                Err(e) => {
                    error!("Couldn't save playlist: {e}");
                    self.show_toast("Couldn't save playlist");
                }
            },
        }
    }

    fn toggle_output(&mut self) {
        self.set_output(self.config.output.toggle());
        self.show_toast(format!("Audio: {}", self.config.output.label()));
    }

    /// Switch output. The current song moves over right away and carries on
    /// from where it was (paused stays paused).
    fn set_output(&mut self, output: Output) {
        if self.config.output == output {
            return;
        }
        self.config.output = output;
        self.player.set_system_audio(self.uses_system_audio());

        if let Some(i) = self.current {
            if let Some(song) = self.queue.get(i) {
                let paused = self.player.is_paused();
                let position = self.player.position();
                self.player.play_from(song.path.clone(), position);
                if paused {
                    self.player.toggle_pause();
                }
            }
        }

        save_conf(&self.config);
    }

    /// Follow Bluetooth connection changes: headphones connecting take over
    /// the audio, and losing them pauses playback (instead of carrying on
    /// out of another device) and goes back to wired.
    fn handle_bluetooth(&mut self) {
        let on_bt_screen = matches!(self.gui(), GUI::Settings { .. } | GUI::Bluetooth { .. });
        self.bluetooth.poll(on_bt_screen);

        for event in self.bluetooth.take_events() {
            match event {
                BtEvent::Connected(name) => {
                    info!("Bluetooth connected: {name}");
                    // They become the output with whatever volume they had:
                    // put the saved system volume back on them.
                    self.schedule_system_volume(Duration::from_secs(2), 1);
                    self.set_output(Output::Wireless);
                    self.show_toast(format!("Audio: {}", truncate_name(&name)));
                }
                BtEvent::Disconnected(name) => {
                    info!("Bluetooth disconnected: {name}");
                    let still_connected = self.bluetooth.devices().iter().any(|d| d.connected);
                    if self.config.output == Output::Wireless && !still_connected {
                        if self.current.is_some() && !self.player.is_paused() {
                            self.player.toggle_pause();
                        }
                        self.set_output(Output::Wired);
                    }
                    self.show_toast(format!("{} disconnected", truncate_name(&name)));
                }
            }
        }
    }

    /// Automatic update checks, and toasts for what the updater did.
    fn handle_updates(&mut self) {
        self.updater.poll();
        for event in self.updater.take_events() {
            match event {
                UpdateEvent::Found(tag) => {
                    if self.update_dismissed.as_ref() != Some(&tag) {
                        self.update_prompt = Some(tag);
                    }
                }
                UpdateEvent::UpToDate => self.show_toast(format!("Up to date (v{VERSION})")),
                UpdateEvent::Failed(e) => self.show_error(e),
                UpdateEvent::Installed(tag) => {
                    if self.current.is_some() && !self.player.is_paused() {
                        // Don't cut the song off: restart when it changes.
                        self.restart_after_song = true;
                        self.show_toast_for(format!("{tag} ready: restarts after this song"), ERROR_TOAST_DURATION);
                    } else {
                        self.show_toast(format!("Restarting into {tag}"));
                        self.restart_now("update");
                    }
                }
            }
        }

        // Ask right away, waking the screen if it's asleep, but don't
        // interrupt typing or the help overlay.
        if self.update_prompt.is_some()
            && !self.help
            && !matches!(self.gui(), GUI::TextInput { .. } | GUI::UpdatePrompt { .. })
            && matches!(self.updater.state(), UpdateState::Available(_))
        {
            self.update_prompt = None;
            self.idle = false;
            self.last_input = Instant::now();
            self.nav.push(GUI::UpdatePrompt { selected: 0 });
            self.reset_anim();
        }
    }

    /// What the device is doing, for the line under every screen's title:
    /// update / restart / shutdown in progress first, else the song.
    fn activity(&self) -> Option<String> {
        match self.updater.state() {
            UpdateState::Installing => return Some("Installing update...".into()),
            UpdateState::Installed if self.restart_after_song => {
                return Some("Update ready: restarts after this song".into());
            }
            UpdateState::Installed => return Some("Restarting into the new version...".into()),
            _ => {}
        }
        match self.root_job {
            Some((_, RootJob::Power(PowerAction::Reboot))) => return Some("Restarting device...".into()),
            Some((_, RootJob::Power(PowerAction::PowerOff))) => return Some("Shutting down...".into()),
            _ => {}
        }
        if self.restart_at.is_some() {
            return Some("Restarting bman...".into());
        }
        if self.usb.as_ref().is_some_and(|u| u.busy()) {
            return Some("USB: copying files...".into());
        }
        // Now Playing shows the song itself.
        if matches!(self.gui(), GUI::NowPlaying) {
            return None;
        }
        let song = self.queue.get(self.current?)?;
        let state = if self.player.is_paused() { "Paused" } else { "Playing" };
        Some(match &song.artist {
            Some(artist) => format!("{state}: {} - {artist}", song.title),
            None => format!("{state}: {}", song.title),
        })
    }

    /// Settings "System" row value.
    fn system_value(&self) -> String {
        match self.updater.state() {
            UpdateState::Available(_) => "update!".into(),
            UpdateState::Installing => "updating".into(),
            _ => format!("v{VERSION}"),
        }
    }

    /// Rows of the System screen and its status line.
    fn system_rows(&self) -> (Vec<String>, String) {
        let update = match self.updater.state() {
            UpdateState::Checking => "Checking...".to_string(),
            UpdateState::Available(r) if self.updater.install_error().is_some() => format!("Retry install {}", r.tag),
            UpdateState::Available(r) => format!("Install {}", r.tag),
            UpdateState::Installing => "Installing...".into(),
            UpdateState::Installed if self.restart_after_song => "Restart now (update ready)".into(),
            UpdateState::Installed => "Restarting...".into(),
            UpdateState::UpToDate => "Up to date".into(),
            UpdateState::Idle | UpdateState::Failed => "Check for updates".into(),
        };
        let on_off = |b: bool| if b { "On" } else { "Off" };
        let boot = match self.init {
            Some(_) if matches!(self.root_job, Some((_, RootJob::Boot(_)))) => {
                "Start on boot: ...".into()
            }
            Some(_) => format!("Start on boot: {}", on_off(self.boot_enabled)),
            None => "Start on boot: n/a".into(),
        };
        let status = match (self.updater.install_error(), self.init) {
            (Some(e), _) if matches!(self.updater.state(), UpdateState::Available(_)) => {
                format!("Download failed: {e}")
            }
            _ if self.usb_note.is_some() && self.config.usb && self.usb.is_none() => {
                self.usb_note.clone().unwrap_or_default()
            }
            (_, Some(init)) => format!("bman v{VERSION} ({})", init.name()),
            (_, None) => format!("bman v{VERSION}"),
        };
        let armed = |row: usize| {
            self.power_armed
                .is_some_and(|(r, t)| r == row && t.elapsed() < CONFIRM_WINDOW)
        };
        let busy = |a: PowerAction| matches!(self.root_job, Some((_, RootJob::Power(b))) if b == a);
        let restart = if busy(PowerAction::Reboot) {
            "Restarting...".into()
        } else if armed(ROW_RESTART) {
            "Press 1 again: restart".into()
        } else {
            "Restart device".to_string()
        };
        let shutdown = if busy(PowerAction::PowerOff) {
            "Shutting down...".into()
        } else if armed(ROW_SHUTDOWN) {
            "Press 1 again: shut down".into()
        } else {
            "Shut down".to_string()
        };
        (
            vec![
                update,
                format!("Auto check: {}", on_off(self.updater.auto_check())),
                boot,
                restart,
                shutdown,
                "Restart bman".into(),
                self.usb_row(),
            ],
            status,
        )
    }

    /// The tag the update prompt offers.
    fn offered_tag(&self) -> String {
        match self.updater.state() {
            UpdateState::Available(r) => r.tag,
            _ => String::new(),
        }
    }

    /// Press on the System screen's update row: check, or install if found.
    fn update_action(&mut self) {
        match self.updater.state() {
            UpdateState::Available(r) => {
                self.show_toast(format!("Downloading {}", r.tag));
                self.updater.install();
            }
            // Waiting for the song to end: restart now instead.
            UpdateState::Installed if self.restart_after_song => self.restart_now("update"),
            UpdateState::Checking | UpdateState::Installing | UpdateState::Installed => {}
            _ => self.updater.check(true),
        }
    }

    fn toggle_boot(&mut self) {
        if self.root_job.is_some() {
            return;
        }
        if self.init.is_none() {
            self.show_error("Unknown init system");
            return;
        }
        self.start_root_job(RootJob::Boot(!self.boot_enabled), None);
    }

    // --- system volume -------------------------------------------------

    fn read_system_volume(&mut self) {
        self.system_volume_now = mixer::get().ok();
        self.system_volume_read_at = Instant::now();
    }

    /// Set the saved system volume again soon (after startup, or when
    /// headphones connect and become the output).
    fn schedule_system_volume(&mut self, delay: Duration, repeats: u8) {
        if self.config.system_volume.is_some() {
            self.system_volume_apply = Some((Instant::now() + delay, 10, repeats));
        }
    }

    fn poll_system_volume(&mut self) {
        if let Some((at, tries, repeats)) = self.system_volume_apply {
            if Instant::now() >= at {
                let target = self.config.system_volume.unwrap_or(50);
                let retry_at = Instant::now() + Duration::from_secs(3);
                self.system_volume_apply = match mixer::set(target) {
                    Ok(()) => {
                        info!("System volume set to {target}%");
                        self.read_system_volume();
                        (repeats > 0).then_some((retry_at, tries, repeats - 1))
                    }
                    // Sound server not up yet (e.g. at boot): try again.
                    Err(_) if tries > 1 => Some((retry_at, tries - 1, repeats)),
                    Err(e) => {
                        warn!("Couldn't set system volume: {e}");
                        None
                    }
                };
            }
        }
        let on_volume = matches!(self.gui(), GUI::Settings { .. } | GUI::Volume { .. });
        if on_volume && self.system_volume_read_at.elapsed() >= Duration::from_secs(2) {
            self.read_system_volume();
        }
    }

    fn adjust_system_volume(&mut self, up: bool) {
        let current = self.system_volume_now.or(self.config.system_volume).unwrap_or(50);
        let next = (current as i32 + if up { 5 } else { -5 }).clamp(0, 100) as u8;
        match mixer::set(next) {
            Ok(()) => {
                self.config.system_volume = Some(next);
                self.system_volume_now = Some(next);
                self.system_volume_read_at = Instant::now();
            }
            Err(e) => self.show_error(e),
        }
    }

    /// If a radio is blocked by rfkill, unblock it (through sudo). `prompt`:
    /// ask for the sudo password if needed (when its screen is opened).
    fn ensure_unblocked(&mut self, radio: Radio, prompt: bool) {
        match rfkill::state(radio) {
            rfkill::Block::None => {}
            rfkill::Block::Hard => {
                if prompt {
                    let msg = format!("{} off by hardware switch", radio.label());
                    self.show_error(msg);
                }
            }
            rfkill::Block::Soft => {
                if self.root_job.is_none() {
                    info!("{} is blocked (rfkill), unblocking", radio.label());
                    self.start_root_job(RootJob::Unblock { radio, prompt }, None);
                }
            }
        }
    }

    // --- podcasts and audiobooks -----------------------------------------------

    fn current_is_spoken(&self) -> bool {
        self.current
            .and_then(|i| self.queue.get(i))
            .is_some_and(|s| spoken::is_spoken(&s.path))
    }

    /// Remember where the playing podcast / audiobook is.
    fn save_spoken_position(&mut self) {
        let Some(path) = self.spoken_playing.clone() else {
            return;
        };
        let (pos, len) = (self.player.position_secs(), self.player.length_secs());
        // Not started yet, or already ended (on_track_end marks it done).
        if pos == 0 && len == 0 {
            return;
        }
        self.progress.set(&path, pos, len);
        self.progress.save();
        self.spoken_saved = Instant::now();
    }

    /// Jump forward / back within the playing podcast or chapter.
    fn seek_spoken(&mut self, delta: i64) {
        let Some(i) = self.current else { return };
        let len = self.player.length_secs() as i64;
        let mut pos = self.player.position_secs() as i64 + delta;
        if len > 0 {
            pos = pos.min(len - 1);
        }
        let paused = self.player.is_paused();
        self.play_index_from(i, Duration::from_secs(pos.max(0) as u64));
        if paused {
            self.player.toggle_pause();
        }
        self.show_toast(if delta > 0 { format!("+{delta} s") } else { format!("{delta} s") });
    }

    /// 5 on Now Playing for spoken audio: the next speed (applies at once).
    fn cycle_speed(&mut self) {
        let next = SPEEDS
            .iter()
            .position(|&s| (s - self.config.spoken_speed).abs() < 0.01)
            .map_or(0, |i| (i + 1) % SPEEDS.len());
        self.config.spoken_speed = SPEEDS[next];
        save_conf(&self.config);
        self.show_toast(format!("Speed: {}x", format_speed(SPEEDS[next])));
        if let Some(i) = self.current {
            let (pos, paused) = (self.player.position(), self.player.is_paused());
            self.play_index_from(i, pos);
            if paused {
                self.player.toggle_pause();
            }
        }
    }

    /// Downloads finishing, rescans, saving the position, feed refreshes.
    fn poll_spoken(&mut self) {
        if self.spoken.take_changed() {
            self.request_rescan();
        }
        for (file, result) in self.spoken.take_finished() {
            let waiting = self.spoken_wait.as_ref().is_some_and(|s| s.path == file);
            match result {
                Ok(()) if waiting => {
                    if let Some(song) = self.spoken_wait.take() {
                        let start = self.progress.resume_at(&song.path);
                        self.queue = vec![song];
                        self.reset_shuffle();
                        self.play_index_from(0, start);
                    }
                }
                Err(e) if waiting => {
                    self.spoken_wait = None;
                    self.show_error(format!("Download failed: {e}"));
                }
                _ => {}
            }
        }
        let playing = self.current_is_spoken() && !self.player.is_paused();
        if playing && self.spoken_saved.elapsed() >= Duration::from_secs(15) {
            self.save_spoken_position();
        }
        let due = match self.feeds_refreshed {
            None => self.started.elapsed() >= spoken::REFRESH_FIRST,
            Some(t) => t.elapsed() >= spoken::REFRESH_EVERY,
        };
        if due {
            self.feeds_refreshed = Some(Instant::now());
            if self.spoken.podcasts().join(spoken::FEEDS_FILE).is_file() {
                self.spoken.refresh();
            }
        }
    }

    fn episode_label(&self, e: &spoken::Episode) -> String {
        let mark = self.progress.get(&e.path);
        let state = if let Some(p) = self.spoken.progress(&e.path) {
            format!("[{p}%]")
        } else if !e.downloaded() {
            "[get]".into()
        } else if mark.done {
            "[done]".into()
        } else if mark.secs > 0 {
            format!("[{}]", clock(mark.secs))
        } else {
            "[new]".into()
        };
        let date = e.date.get(5..).map(|md| format!("{md} ")).unwrap_or_default();
        format!("{state} {date}{}", e.title)
    }

    /// (title, status line, rows) of the podcast / audiobook screen showing.
    fn spoken_view(&self) -> (String, String, Vec<String>) {
        let lib = &self.library;
        let status = self.spoken.status();
        let or = |s: String, other: String| if s.is_empty() { other } else { s };
        match self.gui() {
            GUI::Podcasts { .. } => {
                let mut rows = vec!["Search podcasts".to_string(), "Refresh".to_string()];
                for s in &lib.shows {
                    let new = s
                        .episodes
                        .iter()
                        .filter(|e| e.downloaded() && self.progress.get(&e.path).when == 0)
                        .count();
                    rows.push(if new > 0 { format!("{} ({new} new)", s.title) } else { s.title.clone() });
                }
                let armed = self.unsubscribe_armed.filter(|(_, t)| t.elapsed() < CONFIRM_WINDOW);
                let status = match armed {
                    Some(_) => "Press 5 again: unsubscribe".to_string(),
                    None => or(status, format!("{} shows", lib.shows.len())),
                };
                ("Podcasts".into(), status, rows)
            }
            GUI::Show { show, .. } => match lib.shows.get(*show) {
                Some(s) => {
                    let rows: Vec<String> = s.episodes.iter().map(|e| self.episode_label(e)).collect();
                    let status = format!("{} episodes. 5: played", s.episodes.len());
                    (s.title.clone(), status, rows)
                }
                None => ("Podcast".into(), String::new(), Vec::new()),
            },
            GUI::PodcastResults { .. } => {
                let rows = self.spoken.podcast_results().iter().map(|f| f.label()).collect();
                ("Podcasts found".into(), or(status, "1: subscribe".into()), rows)
            }
            GUI::Audiobooks { .. } => {
                let mut rows = vec!["Search LibriVox".to_string()];
                for b in &lib.books {
                    rows.push(match self.progress.percent(&b.files) {
                        Some(100) => format!("{} (done)", b.label()),
                        Some(p) => format!("{} ({p}%)", b.label()),
                        None => b.label(),
                    });
                }
                let downloading = lib.books.iter().flat_map(|b| &b.files).any(|f| self.spoken.progress(f).is_some());
                let status = if downloading || self.spoken.busy() {
                    status
                } else {
                    format!("{} books. 5: finished", lib.books.len())
                };
                ("Audiobooks".into(), status, rows)
            }
            GUI::Book { book, .. } => match lib.books.get(*book) {
                Some(b) => {
                    let resume = match self.progress.last_of(&b.files) {
                        Some((i, pos)) => {
                            let chapter = self
                                .book_chapters
                                .iter()
                                .rev()
                                .find(|c| c.file == b.files[i] && c.start <= pos)
                                .map(|c| c.title.clone())
                                .unwrap_or_default();
                            format!("Resume: {chapter} {}", clock(pos.as_secs()))
                        }
                        None => "Start".into(),
                    };
                    let mut rows = vec![resume];
                    rows.extend(self.book_chapters.iter().map(|c| c.title.clone()));
                    let by = b.author.clone().unwrap_or_default();
                    (b.title.clone(), by, rows)
                }
                None => ("Audiobook".into(), String::new(), Vec::new()),
            },
            GUI::BookResults { .. } => {
                let rows = self.spoken.book_results().iter().map(|f| f.label()).collect();
                ("LibriVox".into(), or(status, "1: download".into()), rows)
            }
            _ => (String::new(), String::new(), Vec::new()),
        }
    }

    fn play_episode(&mut self, show: usize, episode: usize) -> bool {
        let Some(s) = self.library.shows.get(show) else { return false };
        let Some(e) = s.episodes.get(episode).cloned() else { return false };
        let song = Song {
            title: e.title.clone(),
            track: None,
            path: e.path.clone(),
            artist: Some(s.title.clone()),
            album: s.title.clone(),
        };
        if e.downloaded() {
            let start = self.progress.resume_at(&e.path);
            self.queue = vec![song];
            self.reset_shuffle();
            self.play_index_from(0, start);
            true
        } else if let Some(url) = &e.url {
            self.spoken.download(url, &e.path);
            self.spoken_wait = Some(song);
            self.show_toast("Downloading: plays when ready");
            false
        } else {
            false
        }
    }

    /// Row 0 resumes (or starts) the book, row N plays chapter N.
    fn play_book(&mut self, book: usize, row: usize) {
        let Some(b) = self.library.books.get(book).cloned() else { return };
        let songs: Vec<Song> = b
            .files
            .iter()
            .map(|f| Song {
                title: if b.files.len() == 1 {
                    b.title.clone()
                } else {
                    f.file_stem().unwrap_or_default().to_string_lossy().into_owned()
                },
                track: None,
                path: f.clone(),
                artist: b.author.clone(),
                album: b.title.clone(),
            })
            .collect();
        let (index, start) = match row.checked_sub(1).and_then(|r| self.book_chapters.get(r)) {
            Some(ch) => (b.files.iter().position(|f| *f == ch.file).unwrap_or(0), ch.start),
            None => self.progress.last_of(&b.files).unwrap_or((0, Duration::ZERO)),
        };
        self.queue = songs;
        self.reset_shuffle();
        self.play_index_from(index, start);
    }

    fn spoken_button(&mut self, button: Button) -> Nav {
        let gui = self.gui().clone();
        let count = self.spoken_view().2.len();
        let selected = match gui {
            GUI::Podcasts { selected }
            | GUI::Show { selected, .. }
            | GUI::PodcastResults { selected }
            | GUI::Audiobooks { selected }
            | GUI::Book { selected, .. }
            | GUI::BookResults { selected } => selected,
            _ => 0,
        };
        let with = |s: usize| match gui {
            GUI::Podcasts { .. } => GUI::Podcasts { selected: s },
            GUI::Show { show, .. } => GUI::Show { show, selected: s },
            GUI::PodcastResults { .. } => GUI::PodcastResults { selected: s },
            GUI::Audiobooks { .. } => GUI::Audiobooks { selected: s },
            GUI::Book { book, .. } => GUI::Book { book, selected: s },
            _ => GUI::BookResults { selected: s },
        };
        if let Some(nav) = move_selection(button, selected, count, with) {
            self.unsubscribe_armed = None;
            return nav;
        }
        match (gui, button) {
            (GUI::Podcasts { .. }, Button::Select) => match selected {
                0 => return Nav::Push(self.open_keyboard(InputPurpose::PodcastSearch)),
                1 => {
                    self.feeds_refreshed = Some(Instant::now());
                    self.spoken.refresh();
                }
                n if n - 2 < self.library.shows.len() => return Nav::Push(GUI::Show { show: n - 2, selected: 0 }),
                _ => {}
            },
            (GUI::Podcasts { .. }, Button::Action) if selected >= 2 => {
                let Some(show) = self.library.shows.get(selected - 2).cloned() else { return Nav::Stay };
                let Some(feed) = show.feed else {
                    self.show_toast("Not from a feed");
                    return Nav::Stay;
                };
                let armed = self.unsubscribe_armed.is_some_and(|(r, t)| r == selected && t.elapsed() < CONFIRM_WINDOW);
                if armed {
                    self.unsubscribe_armed = None;
                    self.spoken.unsubscribe(&feed);
                    self.show_toast(format!("Unsubscribed from {} (files kept)", show.title));
                } else {
                    self.unsubscribe_armed = Some((selected, Instant::now()));
                }
            }
            (GUI::Show { show, .. }, Button::Select) => {
                if self.play_episode(show, selected) {
                    return Nav::Push(GUI::NowPlaying);
                }
            }
            (GUI::Show { show, .. }, Button::Action) => {
                if let Some(e) = self.library.shows.get(show).and_then(|s| s.episodes.get(selected)) {
                    let done = !self.progress.get(&e.path).done;
                    self.progress.set_done(&e.path, done);
                    self.progress.save();
                    self.show_toast(if done { "Marked played" } else { "Marked not played" });
                }
            }
            (GUI::PodcastResults { .. }, Button::Select) => {
                if let Some(found) = self.spoken.podcast_results().get(selected) {
                    self.spoken.subscribe(&found.feed);
                    self.show_toast(format!("Subscribing to {}", found.title));
                }
            }
            (GUI::Audiobooks { .. }, Button::Select) => {
                if selected == 0 {
                    return Nav::Push(self.open_keyboard(InputPurpose::BookSearch));
                }
                if let Some(b) = self.library.books.get(selected - 1) {
                    self.book_chapters = spoken::chapters(b);
                    return Nav::Push(GUI::Book { book: selected - 1, selected: 0 });
                }
            }
            (GUI::Audiobooks { .. }, Button::Action) if selected >= 1 => {
                if let Some(b) = self.library.books.get(selected - 1).cloned() {
                    let done = self.progress.percent(&b.files) != Some(100);
                    for f in &b.files {
                        self.progress.set_done(f, done);
                    }
                    self.progress.save();
                    self.show_toast(if done { "Marked finished" } else { "Marked not finished" });
                }
            }
            (GUI::Book { book, .. }, Button::Select) => {
                self.play_book(book, selected);
                return Nav::Push(GUI::NowPlaying);
            }
            (GUI::BookResults { .. }, Button::Select) => {
                if let Some(found) = self.spoken.book_results().get(selected) {
                    self.spoken.get_book(found);
                    self.show_toast(format!("Downloading {} chapters", found.chapters.len()));
                }
            }
            _ => {}
        }
        Nav::Stay
    }

    // --- USB transfer -----------------------------------------------------------

    fn usb_row(&self) -> String {
        if !usb::supported() {
            return "USB transfer: n/a".into();
        }
        if !self.config.usb {
            return "USB transfer: Off".into();
        }
        match &self.usb {
            Some(u) if u.connected() => "USB transfer: Connected".into(),
            Some(_) => "USB transfer: On".into(),
            None if matches!(self.root_job, Some((_, RootJob::Usb { .. }))) => "USB transfer: ...".into(),
            None => "USB transfer: Set up".into(),
        }
    }

    /// Get the port ready (through the sudo helper unless it already is).
    fn usb_setup(&mut self, prompt: bool) {
        if usb::ready() {
            self.start_usb(prompt);
        } else if !usb::supported() {
            if prompt {
                self.show_error("No USB device port here");
            }
        } else if self.root_job.is_some() {
            self.usb_setup_pending = Some(prompt);
        } else {
            self.start_root_job(RootJob::Usb { prompt }, None);
        }
    }

    fn start_usb(&mut self, announce: bool) {
        if let Some(u) = self.usb.as_mut() {
            match u.set_enabled(true) {
                Ok(()) if announce => self.show_toast("USB transfer on"),
                Ok(()) => {}
                Err(e) => self.show_error(e),
            }
            return;
        }
        let music = PathBuf::from(MUSIC_DIR);
        let videos = music.with_file_name(VIDEOS_DIR);
        let podcasts = music.with_file_name(spoken::PODCASTS_DIR);
        let books = music.with_file_name(spoken::AUDIOBOOKS_DIR);
        let folders = vec![("Music", music), ("Videos", videos), ("Podcasts", podcasts), ("Audiobooks", books)];
        match usb::Usb::start(folders, self.usb_status.clone()) {
            Ok(u) => {
                self.usb = Some(u);
                if announce {
                    self.show_toast("USB transfer on");
                }
            }
            Err(e) => {
                warn!("{e}");
                self.usb_note = Some(e.clone());
                self.show_error(e);
            }
        }
    }

    fn toggle_usb(&mut self) {
        let on = self.usb.as_ref().is_some_and(|u| u.enabled());
        if self.config.usb && on {
            self.config.usb = false;
            if let Some(u) = self.usb.as_mut() {
                if let Err(e) = u.set_enabled(false) {
                    self.show_error(e);
                }
            }
            self.show_toast("USB transfer off");
        } else {
            self.config.usb = true;
            self.usb_setup(true);
        }
        save_conf(&self.config);
    }

    /// Rescan after a computer changed files, and run a USB setup that was
    /// waiting for another root job.
    fn poll_usb(&mut self) {
        if self.usb_status.changed.load(Ordering::Relaxed) && !self.usb_status.busy.load(Ordering::Relaxed) {
            self.usb_status.changed.store(false, Ordering::Relaxed);
            self.request_rescan();
        }
        if self.root_job.is_none() {
            if let Some(prompt) = self.usb_setup_pending.take() {
                self.usb_setup(prompt);
            }
        }
        // The boot step may finish after bman started: pick it up then (not
        // after an error, which would only repeat).
        let waiting = self.config.usb && self.usb.is_none() && self.root_job.is_none();
        if waiting && self.usb_checked.elapsed() >= Duration::from_secs(3) {
            self.usb_checked = Instant::now();
            if usb::ready() && self.usb_note.as_deref().is_none_or(|n| n.starts_with("USB: press")) {
                self.usb_note = None;
                self.start_usb(false);
            }
        }
    }

    /// Restart / shut down: the first press arms it, the second does it.
    fn power_press(&mut self, row: usize, action: PowerAction) {
        if self.root_job.is_some() {
            return;
        }
        let armed = self
            .power_armed
            .is_some_and(|(r, t)| r == row && t.elapsed() < CONFIRM_WINDOW);
        if armed {
            self.power_armed = None;
            save_conf(&self.config);
            self.start_root_job(RootJob::Power(action), None);
        } else {
            self.power_armed = Some((row, Instant::now()));
        }
    }

    /// Run a root job on a thread (it may go through sudo).
    fn start_root_job(&mut self, job: RootJob, password: Option<String>) {
        let init = self.init;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            crate::sched::background(); // never compete with playback
            let result = match job {
                RootJob::Boot(on) => match init {
                    Some(init) => autostart::set(init, on, password.as_deref()),
                    None => Err(RootError::Failed("Unknown init system".into())),
                },
                RootJob::Power(action) => power::request(action, password.as_deref()),
                RootJob::Unblock { radio, .. } => rfkill::unblock(radio, password.as_deref()),
                RootJob::Usb { .. } => usb::setup(password.as_deref()),
            };
            let _ = tx.send(result);
        });
        self.root_job = Some((rx, job));
    }

    fn poll_root_job(&mut self) {
        let Some((rx, job)) = &self.root_job else { return };
        let job = *job;
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(RootError::Failed("Failed".into())),
        };
        self.root_job = None;

        match result {
            Ok(msg) => {
                info!("{msg}");
                if matches!(job, RootJob::Power(_)) {
                    self.powering_off = true;
                }
                if let RootJob::Usb { prompt } = job {
                    if usb::ready() {
                        self.usb_note = None;
                        self.start_usb(prompt);
                    } else {
                        // e.g. the Pi's overlay was just added: restart needed.
                        self.usb_note = Some(msg.clone());
                        self.show_toast_for(msg, ERROR_TOAST_DURATION);
                    }
                } else if let RootJob::Unblock { radio, .. } = job {
                    match radio {
                        Radio::Bluetooth => self.bluetooth.power_up(),
                        Radio::Wifi => self.network.set_wifi_enabled(true),
                    }
                    self.show_toast(msg);
                } else {
                    self.show_toast_for(msg, ERROR_TOAST_DURATION);
                }
            }
            // The silent startup try doesn't ask; opening the screen will.
            Err(RootError::NeedsPassword | RootError::WrongPassword)
                if matches!(job, RootJob::Unblock { prompt: false, .. }) =>
            {
                info!("Radio blocked; will ask for the sudo password when its screen is opened");
            }
            Err(RootError::NeedsPassword | RootError::WrongPassword)
                if matches!(job, RootJob::Usb { prompt: false }) =>
            {
                info!("USB transfer needs the sudo password: Settings > System > USB transfer");
                self.usb_note = Some("USB: press to enter the sudo password".into());
            }
            Err(RootError::Failed(e)) if matches!(job, RootJob::Usb { .. }) => {
                warn!("USB setup: {e}");
                self.usb_note = Some(format!("USB: {e}"));
                self.show_error(e);
            }
            Err(RootError::NeedsPassword) => {
                let gui = self.open_keyboard(InputPurpose::SudoPassword { job });
                self.nav.push(gui);
            }
            Err(RootError::WrongPassword) => {
                self.show_error("Wrong password");
                let gui = self.open_keyboard(InputPurpose::SudoPassword { job });
                self.nav.push(gui);
            }
            Err(RootError::Failed(e)) => {
                warn!("{e}");
                self.show_error(e);
            }
        }
        if let Some(init) = self.init {
            self.boot_enabled = autostart::is_enabled(init);
        }
    }

    /// Don't let the screen sleep while an update is being asked about,
    /// downloaded or installed, so the prompt and the restart stay visible.
    fn keep_awake(&self) -> bool {
        let state = self.updater.state();
        matches!(self.gui(), GUI::UpdatePrompt { .. })
            // Settings > System offering "Install vX.Y.Z".
            || (matches!(self.gui(), GUI::System { .. })
                && matches!(state, UpdateState::Available(_)))
            || matches!(state, UpdateState::Installing)
            // About to restart (not while waiting for the song to end).
            || self.restart_at.is_some()
            // A music video playing on Now Playing (paused: it may sleep).
            || (matches!(self.gui(), GUI::NowPlaying)
                && self.video.is_some()
                && self.current.is_some()
                && !self.player.is_paused())
    }

    fn restart_due(&self) -> bool {
        self.restart_at.is_some_and(|t| Instant::now() >= t)
    }

    fn cycle_battery_driver(&mut self) {
        let next = next_battery_driver(&self.config.battery);
        match create_battery(next) {
            Ok(source) => {
                self.battery.set_source(source);
                self.config.battery = next.to_string();
                save_conf(&self.config);
                self.show_toast(format!("Battery: {next}"));
            }
            Err(e) => self.show_toast(e),
        }
    }

    // --- rescanning the music folder ---------------------------------------

    /// Scan the music folder again on a background thread. If a scan is
    /// already running, run one more when it finishes.
    fn request_rescan(&mut self) {
        // One at a time, and not more often than RESCAN_INTERVAL: a scan
        // stats every file, which competes with playback for the SD card.
        let too_soon = self.last_scan.is_some_and(|t| t.elapsed() < RESCAN_INTERVAL);
        if self.scan_rx.is_some() || too_soon {
            self.rescan_pending = true;
            return;
        }
        self.last_scan = Some(Instant::now());

        let (tx, rx) = mpsc::channel();
        let root = PathBuf::from(MUSIC_DIR);
        thread::spawn(move || {
            crate::sched::background(); // never compete with playback
            let _ = tx.send(Library::scan(&root));
        });

        self.scan_rx = Some((rx, self.library_edits));
    }

    fn poll_scan(&mut self) {
        // A scan asked for too soon after the last one: run it now if due.
        if self.rescan_pending && self.scan_rx.is_none() {
            let due = self.last_scan.is_none_or(|t| t.elapsed() >= RESCAN_INTERVAL);
            if due {
                self.rescan_pending = false;
                self.request_rescan();
            }
        }

        let polled = self
            .scan_rx
            .as_ref()
            .map(|(rx, edits)| (rx.try_recv(), *edits));

        match polled {
            Some((Ok(result), edits)) => {
                self.scan_rx = None;

                match result {
                    // The scan read the playlist files before one of our own
                    // changes was saved, so its copy is stale: scan again.
                    Ok(_) if edits != self.library_edits => self.rescan_pending = true,
                    Ok(library) => self.apply_library(library),
                    Err(e) => {
                        let message = e.to_string();
                        if self.last_scan_error.as_deref() != Some(message.as_str()) {
                            error!("Couldn't rescan music folder: {message}");
                            self.last_scan_error = Some(message);
                        }
                    }
                }

                if self.rescan_pending {
                    self.rescan_pending = false;
                    self.request_rescan();
                }
            }
            Some((Err(TryRecvError::Disconnected), _)) => self.scan_rx = None,
            _ => {}
        }
    }

    /// Swap in a freshly scanned library without disturbing what's on screen.
    fn apply_library(&mut self, new: Library) {
        let old = std::mem::replace(&mut self.library, new);
        self.last_scan_error = None;

        // Only mention problems that weren't there on the last scan.
        for warning in &self.library.warnings {
            if !old.warnings.contains(warning) {
                warn!("{warning}");
            }
        }

        // The playlist that 5 presses add to.
        self.target = self
            .target
            .and_then(|t| old.playlists.get(t))
            .and_then(|p| self.library.playlists.iter().position(|q| q.path == p.path));

        // Keep each open screen pointing at the same album/artist/playlist.
        let mut screens = Vec::with_capacity(self.nav.len());
        for gui in self.nav.drain(..) {
            match remap_gui(gui, &old, &self.library) {
                Some(gui) => screens.push(gui),
                None => break, // it was deleted: drop it and anything opened from it
            }
        }
        if screens.is_empty() {
            screens.push(GUI::Menu { selected: 0 });
        }
        self.nav = screens;
    }

    /// No input for a while: turn the screen off. If a song is playing, show
    /// the now-playing screen so the next press controls it; otherwise go to
    /// the main menu.
    fn enter_idle(&mut self) {
        if self.nav.iter().any(|g| matches!(g, GUI::Settings { .. })) {
            save_conf(&self.config);
        }

        self.help = false;
        self.nav = if self.current.is_some() {
            vec![GUI::Menu { selected: 0 }, GUI::NowPlaying]
        } else {
            vec![GUI::Menu { selected: 0 }]
        };
        self.idle = true;
        self.reset_anim();
        self.request_rescan();
    }

    /// Fade before sleeping, as a percentage of the brightness setting:
    /// 100 normally, down to SLEEP_DIM_PCT a few seconds before the timeout,
    /// then to 0 just as the screen turns off. Any press brings it back.
    fn sleep_fade_pct(&self) -> u32 {
        let timeout = Duration::from_secs(self.config.screen_timeout as u64);
        // Stays black while asleep, so the backlight doesn't jump back up
        // before it's switched off; a press wakes straight to full.
        if self.idle {
            return 0;
        }
        if self.config.screen_timeout == 0 || self.keep_awake() {
            return 100;
        }
        let left = timeout.saturating_sub(self.last_input.elapsed());
        let frac = |d: Duration, of: Duration| (d.as_secs_f32() / of.as_secs_f32()).clamp(0.0, 1.0);
        if left <= SLEEP_FADE_OUT {
            // SLEEP_DIM_PCT -> 0
            (SLEEP_DIM_PCT as f32 * frac(left, SLEEP_FADE_OUT)).round() as u32
        } else if left <= SLEEP_DIM_BEFORE {
            // 100 -> SLEEP_DIM_PCT
            let t = frac(SLEEP_DIM_BEFORE - left, SLEEP_DIM_RAMP);
            (100.0 - (100 - SLEEP_DIM_PCT) as f32 * t).round() as u32
        } else {
            100
        }
    }

    /// The screen is in the middle of a dim/fade ramp (frames go faster).
    fn fading(&self) -> bool {
        let pct = self.sleep_fade_pct();
        !self.idle && pct < 100 && pct != SLEEP_DIM_PCT
    }

    /// Call once per frame.
    fn update(&mut self) {
        self.poll_scan();
        self.check_battery();
        self.handle_bluetooth();
        self.handle_updates();
        self.poll_root_job();
        self.poll_usb();
        self.poll_spoken();
        self.poll_system_volume();
        self.handle_streams();
        if matches!(
            self.gui(),
            GUI::Settings { .. } | GUI::Network { .. } | GUI::TextInput { .. }
        ) {
            self.network.poll();
        }

        let timeout = Duration::from_secs(self.config.screen_timeout as u64);
        let sleeps = self.config.screen_timeout > 0; // 0 = never sleep
        if sleeps && !self.idle && self.last_input.elapsed() >= timeout && !self.keep_awake() {
            self.enter_idle();
        }
        // The queue ran out while the screen was off: nothing left to control.
        if self.idle && self.current.is_none() && matches!(self.gui(), GUI::NowPlaying) {
            self.nav = vec![GUI::Menu { selected: 0 }];
        }

        match self.player.poll_finished() {
            None => {}
            Some(true) => {
                self.current = None;
                self.art = None;
                self.show_toast("Playback failed");
            }
            Some(false) => self.on_track_end(),
        }
        if self.current.is_none() && self.stream_wait.is_none() {
            self.clean_streams(); // nothing playing: no downloads to keep
        }

        let result = self.art_rx.as_ref().map(|rx| rx.try_recv());
        match result {
            Some(Ok(art)) => {
                self.art = art;
                self.art_rx = None;
            }
            Some(Err(TryRecvError::Disconnected)) => self.art_rx = None,
            _ => {}
        }
    }

    // --- settings -----------------------------------------------------

    /// Change the field being edited. Volume moves in 5% steps, each colour
    /// channel in steps of 15 (so 255 is reachable).
    fn adjust_setting(&mut self, row: usize, editing: u8, up: bool) {
        if row == 0 {
            let pct = self.volume_pct() as i32 + if up { 5 } else { -5 };
            self.config.volume = pct.clamp(0, 100) as f32 / 100.0;
            self.player.set_volume(self.config.volume);
        } else if row == 3 {
            // Brightness: 10% steps, never fully dark.
            let pct = self.config.brightness as i32 + if up { 10 } else { -10 };
            self.config.brightness = pct.clamp(10, 100) as u8;
        } else if row == 4 {
            self.config.screen_timeout = step_timeout(self.config.screen_timeout, up);
        } else {
            // editing 1 = red, 2 = green, 3 = blue
            let shift = 16 - 8 * (editing as u32 - 1);
            let value = ((self.config.accent >> shift) & 0xFF) as i32 + if up { 15 } else { -15 };
            let value = value.clamp(0, 255) as u32;
            self.config.accent = (self.config.accent & !(0xFFu32 << shift)) | (value << shift);
        }
    }

    /// Button press while a settings field is being edited.
    fn edit_setting(&mut self, selected: usize, editing: u8, button: Button) -> Nav {
        let mut next = editing;

        match button {
            Button::Up => self.adjust_setting(selected, editing, true),
            Button::Down => self.adjust_setting(selected, editing, false),
            Button::Select => {
                // Accent has R, G, B; volume and sleep have one field each.
                let last = if selected == 1 { 3 } else { 1 };
                next = if editing >= last { 0 } else { editing + 1 };
                if next == 0 {
                    save_conf(&self.config);
                }
            }
            Button::Back | Button::Action | Button::Help => {}
        }

        Nav::Replace(GUI::Settings {
            selected,
            editing: next,
        })
    }

    // --- on-screen keyboard -------------------------------------------

    fn open_keyboard(&mut self, purpose: InputPurpose) -> GUI {
        self.input_purpose = Some(purpose);
        self.input_text.clear();
        GUI::TextInput {
            selected: 0,
            layout: 0,
        }
    }

    /// (header, subtitle, what OK does) for the keyboard screen.
    fn input_labels(&self) -> (String, String, &'static str) {
        match &self.input_purpose {
            Some(InputPurpose::WifiPassword { ssid }) => ("Password".into(), ssid.clone(), "connect"),
            Some(InputPurpose::PlaylistName { name, .. }) => {
                ("Name".into(), format!("Rename {name} (OK empty: keep)"), "save")
            }
            Some(InputPurpose::StreamSearch { videos }) => {
                let what = if *videos { "YouTube Music videos" } else { "YouTube Music songs" };
                ("Search".into(), what.into(), "search")
            }
            Some(InputPurpose::PodcastSearch) => ("Search".into(), "Podcasts (name of the show)".into(), "search"),
            Some(InputPurpose::BookSearch) => ("Search".into(), "LibriVox (title or author)".into(), "search"),
            Some(InputPurpose::SudoPassword { job }) => {
                let what = match job {
                    RootJob::Boot(_) => "start on boot",
                    RootJob::Power(PowerAction::Reboot) => "restart",
                    RootJob::Power(PowerAction::PowerOff) => "shut down",
                    RootJob::Unblock { radio: Radio::Bluetooth, .. } => "unblock Bluetooth",
                    RootJob::Unblock { radio: Radio::Wifi, .. } => "unblock Wi-Fi",
                    RootJob::Usb { .. } => "USB transfer",
                };
                ("Password".into(), format!("sudo password ({what})"), "continue")
            }
            None => ("Type".into(), String::new(), "finish"),
        }
    }

    /// "OK" on the keyboard.
    fn submit_input(&mut self) {
        let mut then_open = None;
        match self.input_purpose.clone() {
            Some(purpose @ (InputPurpose::PodcastSearch | InputPurpose::BookSearch)) => {
                let query = self.input_text.trim().to_string();
                if query.is_empty() {
                    self.show_toast("Type something");
                    return;
                }
                if matches!(purpose, InputPurpose::PodcastSearch) {
                    self.spoken.search_podcasts(query);
                    then_open = Some(GUI::PodcastResults { selected: 0 });
                } else {
                    self.spoken.search_books(query);
                    then_open = Some(GUI::BookResults { selected: 0 });
                }
            }
            Some(InputPurpose::StreamSearch { videos }) => {
                let query = self.input_text.trim().to_string();
                if query.is_empty() {
                    self.show_toast("Type something");
                    return;
                }
                self.youtube.search(query, videos);
                then_open = Some(GUI::StreamResults { list: StreamList::Results, selected: 0 });
            }
            Some(InputPurpose::WifiPassword { ssid }) => {
                if self.input_text.chars().count() < 8 {
                    self.show_toast("Needs 8+ characters");
                    return;
                }
                let password = std::mem::take(&mut self.input_text);
                self.network.connect_with_password(ssid, password);
            }
            Some(InputPurpose::PlaylistName { path, name }) => {
                let new_name = self.input_text.trim().to_string();
                if new_name.is_empty() || new_name == name {
                    self.show_toast(format!("Kept {name}"));
                } else {
                    let Some(index) = self.library.playlists.iter().position(|p| p.path == path) else {
                        self.show_toast("Playlist gone");
                        return;
                    };
                    match self.library.rename_playlist(index, &new_name) {
                        Ok(()) => {
                            self.library_edits += 1;
                            self.show_toast(format!("Renamed to {new_name}"));
                        }
                        Err(e) => {
                            self.show_toast(e); // stay so it can be fixed
                            return;
                        }
                    }
                }
            }
            Some(InputPurpose::SudoPassword { job }) => {
                let password = std::mem::take(&mut self.input_text);
                self.start_root_job(job, Some(password));
            }
            None => {}
        }

        self.input_purpose = None;
        self.input_text.clear();
        if self.nav.len() > 1 {
            self.nav.pop();
        }
        if let Some(gui) = then_open {
            self.nav.push(gui);
        }
        self.request_rescan();
    }

    // --- input --------------------------------------------------------

    /// A key typed on a real keyboard while the keyboard screen is open.
    fn handle_typed(&mut self, key: Typed) {
        self.last_input = Instant::now();
        if self.idle {
            self.idle = false;
            return;
        }
        self.reset_anim();
        if self.help {
            self.help = false;
            return;
        }
        if !matches!(self.gui(), GUI::TextInput { .. }) {
            return; // e.g. keys after Enter in the same batch
        }
        match key {
            Typed::Char(c) => self.input_text.push(c),
            Typed::Erase => {
                self.input_text.pop();
            }
            Typed::Enter => self.submit_input(),
        }
    }

    fn handle_button(&mut self, button: Button) {
        self.last_input = Instant::now();

        // A press while the screen is off wakes it. If a song is playing the
        // press also works as a normal control on the now-playing screen
        // (1 = pause/play, 2 = next, ...). Otherwise it only wakes the screen.
        if self.idle {
            self.idle = false;
            if self.current.is_none() || button == Button::Help {
                return;
            }
        }

        self.reset_anim();

        // Long press toggles the help overlay. While it's open, any other
        // button just closes it.
        if button == Button::Help {
            self.help = !self.help;
            return;
        }
        if self.help {
            self.help = false;
            return;
        }

        // Back works the same on every screen.
        if button == Button::Back {
            if matches!(self.gui(), GUI::Settings { .. } | GUI::Volume { .. }) {
                save_conf(&self.config); // keep changes if the Pi is unplugged
            }
            if matches!(self.gui(), GUI::TextInput { .. }) {
                self.input_text.clear(); // don't keep a cancelled password around
                self.input_purpose = None;
            }
            if matches!(self.gui(), GUI::UpdatePrompt { .. }) {
                self.update_dismissed = Some(self.offered_tag()); // same as "Not now"
            }
            if self.nav.len() > 1 {
                self.nav.pop();
                self.request_rescan();
            }
            return;
        }

        let nav = match self.gui().clone() {
            GUI::Menu { selected } => {
                if let Some(nav) = move_selection(button, selected, MENU_ITEMS.len(), |s| {
                    GUI::Menu { selected: s }
                }) {
                    nav
                } else if button == Button::Select {
                    Nav::Push(match selected {
                        0 => GUI::NowPlaying,
                        // Row 0 is "Save as playlist"; start on the playing song.
                        1 => GUI::Queue {
                            selected: self.current.map_or(0, |c| c + 1),
                        },
                        2 => GUI::Singles { selected: 0 },
                        3 => GUI::Albums { selected: 0 },
                        4 => GUI::Artists { selected: 0 },
                        5 => GUI::Playlists { selected: 0 },
                        6 => GUI::Videos { selected: 0 },
                        7 => GUI::Podcasts { selected: 0 },
                        8 => GUI::Audiobooks { selected: 0 },
                        9 => GUI::Streaming { selected: 0 },
                        _ => GUI::Settings {
                            selected: 0,
                            editing: 0,
                        },
                    })
                } else {
                    Nav::Stay
                }
            }

            GUI::Podcasts { .. }
            | GUI::Show { .. }
            | GUI::PodcastResults { .. }
            | GUI::Audiobooks { .. }
            | GUI::Book { .. }
            | GUI::BookResults { .. } => self.spoken_button(button),

            GUI::Singles { selected } => {
                let count = self.library.singles.len();
                if let Some(nav) = move_selection(button, selected, count, |s| GUI::Singles {
                    selected: s,
                }) {
                    nav
                } else if button == Button::Select && count > 0 {
                    let songs = self.library.singles.clone();
                    self.start_queue(songs, selected);
                    Nav::Push(GUI::NowPlaying)
                } else if button == Button::Action {
                    if let Some(song) = self.library.singles.get(selected).cloned() {
                        self.add_to_target(song);
                    }
                    Nav::Stay
                } else {
                    Nav::Stay
                }
            }

            GUI::Videos { selected } => {
                let count = self.library.videos.len();
                if let Some(nav) = move_selection(button, selected, count, |s| GUI::Videos {
                    selected: s,
                }) {
                    nav
                } else if button == Button::Select && count > 0 {
                    let videos = self.library.videos.clone();
                    self.start_queue(videos, selected);
                    Nav::Push(GUI::NowPlaying)
                } else if button == Button::Action {
                    // Queue only: playlists live in the music folder.
                    if let Some(song) = self.library.videos.get(selected).cloned() {
                        self.enqueue(song);
                        self.show_toast("Added to queue");
                    }
                    Nav::Stay
                } else {
                    Nav::Stay
                }
            }

            GUI::Albums { selected } => {
                let count = self.library.albums.len();
                if let Some(nav) = move_selection(button, selected, count, |s| GUI::Albums {
                    selected: s,
                }) {
                    nav
                } else if button == Button::Select && count > 0 {
                    Nav::Push(GUI::Album {
                        album: selected,
                        selected: 0,
                    })
                } else {
                    Nav::Stay
                }
            }

            GUI::Album { album, selected } => {
                let Some(a) = self.library.albums.get(album) else {
                    return;
                };
                let count = a.songs.len();

                if let Some(nav) = move_selection(button, selected, count, |s| GUI::Album {
                    album,
                    selected: s,
                }) {
                    nav
                } else if button == Button::Select && count > 0 {
                    let songs = a.songs.clone();
                    self.start_queue(songs, selected);
                    Nav::Push(GUI::NowPlaying)
                } else if button == Button::Action {
                    if let Some(song) = a.songs.get(selected).cloned() {
                        self.add_to_target(song);
                    }
                    Nav::Stay
                } else {
                    Nav::Stay
                }
            }

            GUI::Artists { selected } => {
                let count = self.library.artists.len();
                if let Some(nav) = move_selection(button, selected, count, |s| GUI::Artists {
                    selected: s,
                }) {
                    nav
                } else if button == Button::Select && count > 0 {
                    Nav::Push(GUI::Artist {
                        artist: selected,
                        selected: 0,
                    })
                } else {
                    Nav::Stay
                }
            }

            GUI::Artist { artist, selected } => {
                let Some(a) = self.library.artists.get(artist) else {
                    return;
                };
                let album_count = a.albums.len();
                let count = album_count + a.singles.len();

                if let Some(nav) = move_selection(button, selected, count, |s| GUI::Artist {
                    artist,
                    selected: s,
                }) {
                    nav
                } else if button == Button::Select && count > 0 {
                    if selected < album_count {
                        Nav::Push(GUI::Album {
                            album: a.albums[selected],
                            selected: 0,
                        })
                    } else {
                        // Play this artist's singles, starting from the chosen one.
                        let songs: Vec<Song> = a
                            .singles
                            .iter()
                            .map(|&i| self.library.singles[i].clone())
                            .collect();
                        self.start_queue(songs, selected - album_count);
                        Nav::Push(GUI::NowPlaying)
                    }
                } else if button == Button::Action && count > 0 {
                    if selected < album_count {
                        self.show_toast("Open the album to add songs");
                    } else {
                        let song = self.library.singles[a.singles[selected - album_count]].clone();
                        self.add_to_target(song);
                    }
                    Nav::Stay
                } else {
                    Nav::Stay
                }
            }

            GUI::Playlists { selected } => {
                let count = self.library.playlists.len() + 1;

                if let Some(nav) = move_selection(button, selected, count, |s| GUI::Playlists {
                    selected: s,
                }) {
                    nav
                } else if button == Button::Select {
                    if selected == 0 {
                        match self.library.create_playlist() {
                            Ok(index) => {
                                self.library_edits += 1;
                                if let Some(top) = self.nav.last_mut() {
                                    *top = GUI::Playlists {
                                        selected: index + 1,
                                    };
                                }
                                // Name it right away; OK with nothing typed keeps "Playlist N".
                                let p = &self.library.playlists[index];
                                let purpose = InputPurpose::PlaylistName {
                                    path: p.path.clone(),
                                    name: p.name.clone(),
                                };
                                Nav::Push(self.open_keyboard(purpose))
                            }
                            Err(e) => {
                                error!("Couldn't create playlist: {e}");
                                self.show_toast("Couldn't create playlist");
                                Nav::Stay
                            }
                        }
                    } else {
                        Nav::Push(GUI::Playlist {
                            playlist: selected - 1,
                            selected: 0,
                        })
                    }
                } else if button == Button::Action && selected > 0 {
                    // Choose which playlist songs get added to.
                    let p = selected - 1;
                    if self.target == Some(p) {
                        self.target = None;
                        self.show_toast("Adding to queue");
                    } else {
                        self.target = Some(p);
                        let name = self.playlist_name(p);
                        self.show_toast(format!("Adding to {name}"));
                    }
                    Nav::Stay
                } else {
                    Nav::Stay
                }
            }

            GUI::Playlist { playlist, selected } => {
                let Some(p) = self.library.playlists.get(playlist) else {
                    return;
                };
                // Row 0 is "Rename playlist", songs start at row 1.
                let count = p.songs.len() + 1;

                if let Some(nav) = move_selection(button, selected, count, |s| GUI::Playlist {
                    playlist,
                    selected: s,
                }) {
                    nav
                } else if button == Button::Select && selected == 0 {
                    let purpose = InputPurpose::PlaylistName {
                        path: p.path.clone(),
                        name: p.name.clone(),
                    };
                    Nav::Push(self.open_keyboard(purpose))
                } else if button == Button::Select {
                    let songs = p.songs.clone();
                    self.start_queue(songs, selected - 1);
                    Nav::Push(GUI::NowPlaying)
                } else if button == Button::Action && selected > 0 {
                    self.library_edits += 1;
                    match self.library.remove_from_playlist(playlist, selected - 1) {
                        Ok(()) => self.show_toast("Removed from playlist"),
                        Err(e) => {
                            error!("Couldn't save playlist: {e}");
                            self.show_toast("Couldn't save playlist");
                        }
                    }
                    let left = self
                        .library
                        .playlists
                        .get(playlist)
                        .map(|p| p.songs.len())
                        .unwrap_or(0);
                    Nav::Replace(GUI::Playlist {
                        playlist,
                        selected: selected.min(left),
                    })
                } else {
                    Nav::Stay
                }
            }

            GUI::Queue { selected } => {
                // Row 0 is "Save as playlist", songs start at row 1.
                let count = self.queue.len() + 1;

                if let Some(nav) = move_selection(button, selected, count, |s| GUI::Queue {
                    selected: s,
                }) {
                    nav
                } else if button == Button::Select && selected == 0 {
                    self.save_queue_as_playlist()
                } else if button == Button::Select {
                    self.play_index(selected - 1);
                    Nav::Push(GUI::NowPlaying)
                } else if button == Button::Action && selected > 0 {
                    self.remove_from_queue(selected - 1);
                    Nav::Replace(GUI::Queue {
                        selected: selected.min(self.queue.len()),
                    })
                } else {
                    Nav::Stay
                }
            }

            GUI::NowPlaying if self.current_is_spoken() => {
                // Podcasts and audiobooks: skip within the file, change speed.
                match button {
                    Button::Select => {
                        self.player.toggle_pause();
                        self.save_spoken_position();
                    }
                    Button::Down => self.seek_spoken(30),
                    Button::Up => self.seek_spoken(-15),
                    Button::Action => self.cycle_speed(),
                    Button::Back | Button::Help => {}
                }
                Nav::Stay
            }

            GUI::NowPlaying => {
                match button {
                    Button::Select => self.player.toggle_pause(),
                    Button::Down => self.skip_next(),
                    Button::Up => self.skip_previous(),
                    Button::Action => {
                        self.loop_mode = self.loop_mode.next();
                        self.show_toast(format!("Loop: {}", self.loop_mode.label()));
                        // The song after this one may have changed.
                        self.reset_shuffle();
                        self.plan_shuffle();
                        self.clean_streams();
                        self.prefetch_next();
                    }
                    Button::Back | Button::Help => {}
                }
                Nav::Stay
            }

            GUI::Settings { selected, editing } => {
                if editing > 0 {
                    self.edit_setting(selected, editing, button)
                } else if let Some(nav) = move_selection(button, selected, SETTINGS_ROWS, |s| {
                    GUI::Settings {
                        selected: s,
                        editing: 0,
                    }
                }) {
                    nav
                } else if button == Button::Select {
                    match selected {
                        0 => {
                            self.read_system_volume();
                            Nav::Push(GUI::Volume {
                                selected: 0,
                                editing: false,
                            })
                        }
                        1 | 3 | 4 => Nav::Replace(GUI::Settings {
                            selected,
                            editing: 1,
                        }),
                        2 => {
                            self.toggle_output();
                            Nav::Stay
                        }
                        5 => {
                            self.cycle_battery_driver();
                            Nav::Stay
                        }
                        6 => {
                            self.network.refresh();
                            Nav::Push(GUI::Network { selected: 0 })
                        }
                        8 => {
                            self.boot_enabled = self.init.is_some_and(autostart::is_enabled);
                            Nav::Push(GUI::System { selected: 0 })
                        }
                        _ => {
                            self.ensure_unblocked(Radio::Bluetooth, true);
                            self.bluetooth.refresh();
                            Nav::Push(GUI::Bluetooth { selected: 0 })
                        }
                    }
                } else {
                    Nav::Stay
                }
            }

            GUI::Bluetooth { selected } => {
                let devices = self.bluetooth.devices();
                let count = devices.len() + 1;

                if let Some(nav) = move_selection(button, selected, count, |s| GUI::Bluetooth {
                    selected: s,
                }) {
                    nav
                } else if button == Button::Select {
                    if selected == 0 {
                        self.ensure_unblocked(Radio::Bluetooth, true);
                        self.bluetooth.scan();
                    } else if let Some(device) = devices.get(selected - 1) {
                        self.bluetooth.activate(device.mac.clone());
                    }
                    Nav::Stay
                } else {
                    Nav::Stay
                }
            }

            GUI::Network { selected } => {
                let networks = self.network.networks();
                let enabled = self.network.wifi_enabled();
                let count = if enabled { networks.len() + 3 } else { 2 };
                let selected = selected.min(count - 1);
                let network = selected.checked_sub(3).and_then(|i| networks.get(i)).cloned();

                if let Some(nav) = move_selection(button, selected, count, |s| GUI::Network {
                    selected: s,
                }) {
                    nav
                } else if button == Button::Select {
                    let mut next = None;
                    match (selected, network) {
                        (0, _) => {
                            // Turning on: lift an rfkill block too (NetworkManager
                            // can't when the system blocked it).
                            if !enabled {
                                self.ensure_unblocked(Radio::Wifi, true);
                            }
                            self.network.set_wifi_enabled(!enabled)
                        }
                        (1, _) => {
                            let ip = self.network.ip().unwrap_or_else(|| "no IP".into());
                            self.show_toast(ip);
                        }
                        (2, _) => self.network.scan(),
                        (_, Some(n)) if n.connected || n.saved || !n.secured => {
                            self.network.activate(n.ssid)
                        }
                        (_, Some(n)) => {
                            next = Some(self.open_keyboard(InputPurpose::WifiPassword {
                                ssid: n.ssid,
                            }));
                        }
                        _ => {}
                    }
                    next.map_or(Nav::Stay, Nav::Push)
                } else if button == Button::Action {
                    match network {
                        Some(n) if n.saved => self.network.forget(n.ssid),
                        Some(_) => self.show_toast("Not a saved network"),
                        None => {}
                    }
                    Nav::Stay
                } else {
                    Nav::Stay
                }
            }

            GUI::Volume { selected, editing } => {
                if editing {
                    match button {
                        Button::Up | Button::Down => {
                            let up = button == Button::Up;
                            if selected == 0 {
                                self.adjust_setting(0, 1, up);
                            } else {
                                self.adjust_system_volume(up);
                            }
                            Nav::Stay
                        }
                        Button::Select => {
                            save_conf(&self.config);
                            Nav::Replace(GUI::Volume { selected, editing: false })
                        }
                        _ => Nav::Stay,
                    }
                } else if let Some(nav) = move_selection(button, selected, 2, |s| GUI::Volume {
                    selected: s,
                    editing: false,
                }) {
                    nav
                } else if button == Button::Select {
                    Nav::Replace(GUI::Volume { selected, editing: true })
                } else {
                    Nav::Stay
                }
            }

            GUI::System { selected } => {
                if let Some(nav) = move_selection(button, selected, SYSTEM_ROWS, |s| GUI::System {
                    selected: s,
                }) {
                    self.power_armed = None;
                    nav
                } else if button == Button::Select {
                    match selected {
                        0 => self.update_action(),
                        1 => {
                            let on = !self.updater.auto_check();
                            self.updater.set_auto_check(on);
                            self.config.update_check = on;
                            save_conf(&self.config);
                        }
                        2 => self.toggle_boot(),
                        ROW_RESTART => self.power_press(ROW_RESTART, PowerAction::Reboot),
                        ROW_SHUTDOWN => self.power_press(ROW_SHUTDOWN, PowerAction::PowerOff),
                        ROW_RESTART_APP => {
                            self.show_toast("Restarting bman");
                            self.restart_now("restart");
                        }
                        ROW_USB => self.toggle_usb(),
                        _ => {}
                    }
                    Nav::Stay
                } else {
                    Nav::Stay
                }
            }

            GUI::Streaming { selected } => {
                if let Some(nav) = move_selection(button, selected, STREAMING_SERVICES.len(), |s| {
                    GUI::Streaming { selected: s }
                }) {
                    nav
                } else if button == Button::Select {
                    Nav::Push(GUI::YouTube { selected: 0 })
                } else {
                    Nav::Stay
                }
            }

            GUI::YouTube { selected } => {
                if let Some(nav) = move_selection(button, selected, YT_ROWS, |s| GUI::YouTube { selected: s }) {
                    nav
                } else if button == Button::Action && selected == YT_ROW_LIKED {
                    self.stream_to_liked = !self.stream_to_liked;
                    self.show_toast(if self.stream_to_liked { "5 on a song: like it" } else { "5 on a song: queue it" });
                    Nav::Stay
                } else if button == Button::Select {
                    let state = self.youtube.state();
                    match selected {
                        YT_ROW_SEARCH | YT_ROW_SEARCH_VIDEOS | YT_ROW_RESULTS if state != ToolState::Ready => {
                            self.show_toast("Set it up first");
                            Nav::Stay
                        }
                        YT_ROW_SEARCH => Nav::Push(self.open_keyboard(InputPurpose::StreamSearch { videos: false })),
                        YT_ROW_SEARCH_VIDEOS => Nav::Push(self.open_keyboard(InputPurpose::StreamSearch { videos: true })),
                        YT_ROW_VIDEOS => {
                            self.config.stream_video = !self.config.stream_video;
                            self.youtube.set_video(self.config.stream_video);
                            save_conf(&self.config);
                            if self.config.stream_video {
                                self.show_toast("Videos on (next songs)");
                            } else {
                                // Only streamed videos; local ones keep playing.
                                if self.current_stream().is_some() {
                                    self.video = None;
                                }
                                self.show_toast("Videos off");
                            }
                            Nav::Stay
                        }
                        YT_ROW_RESULTS => Nav::Push(GUI::StreamResults { list: StreamList::Results, selected: 0 }),
                        YT_ROW_LIKED => Nav::Push(GUI::StreamResults { list: StreamList::Liked, selected: 0 }),
                        YT_ROW_ACCOUNT => {
                            // The saved list shows right away; it's refreshed when it can be.
                            let saved = self.youtube.has_saved_account_likes();
                            let can_refresh = self.youtube.has_cookies() && state == ToolState::Ready;
                            if can_refresh {
                                self.youtube.load_account_likes();
                            }
                            if can_refresh || saved {
                                Nav::Push(GUI::StreamResults { list: StreamList::Account, selected: 0 })
                            } else {
                                let why = if state != ToolState::Ready {
                                    "Set it up first".to_string()
                                } else {
                                    format!("Needs {} in the bman folder", streaming::COOKIES_FILE)
                                };
                                self.show_toast_for(why, ERROR_TOAST_DURATION);
                                Nav::Stay
                            }
                        }
                        YT_ROW_SETUP => {
                            match state {
                                ToolState::Working(_) => self.show_toast("Already working on it"),
                                ToolState::Unsupported => self.show_toast("Needs a 64-bit system"),
                                _ => self.youtube.setup(),
                            }
                            Nav::Stay
                        }
                        _ => Nav::Stay,
                    }
                } else {
                    Nav::Stay
                }
            }

            GUI::StreamResults { list, selected } => {
                let results = self.stream_list(list);
                if let Some(nav) = move_selection(button, selected, results.len(), |s| {
                    GUI::StreamResults { list, selected: s }
                }) {
                    nav
                } else if button == Button::Select && !results.is_empty() {
                    let songs = results.iter().map(|r| self.youtube.song(r)).collect();
                    self.start_queue(songs, selected.min(results.len() - 1));
                    Nav::Push(GUI::NowPlaying)
                } else if button == Button::Action {
                    match results.get(selected) {
                        Some(r) if list == StreamList::Liked => {
                            self.youtube.unlike(&r.id);
                            self.show_toast("Unliked");
                            Nav::Replace(GUI::StreamResults {
                                list,
                                selected: selected.min(results.len().saturating_sub(2)),
                            })
                        }
                        Some(r) if self.stream_to_liked => {
                            let liked = self.youtube.like(r);
                            self.show_toast(if liked { "Liked" } else { "Already liked" });
                            Nav::Stay
                        }
                        Some(r) => {
                            let song = self.youtube.song(r);
                            self.enqueue(song);
                            self.show_toast("Added to queue");
                            Nav::Stay
                        }
                        None => Nav::Stay,
                    }
                } else {
                    Nav::Stay
                }
            }

            GUI::UpdatePrompt { selected } => {
                if let Some(nav) = move_selection(button, selected, 2, |s| GUI::UpdatePrompt {
                    selected: s,
                }) {
                    nav
                } else if button == Button::Select && selected == 0 {
                    self.update_action();
                    self.nav.pop();
                    Nav::Stay
                } else if button == Button::Select {
                    self.update_dismissed = Some(self.offered_tag());
                    self.nav.pop();
                    Nav::Stay
                } else {
                    Nav::Stay
                }
            }

            GUI::TextInput { selected, layout } => {
                let total = key_count(layout);
                match button {
                    Button::Down => Nav::Replace(GUI::TextInput {
                        selected: (selected + 1) % total,
                        layout,
                    }),
                    Button::Up => Nav::Replace(GUI::TextInput {
                        selected: (selected + total - 1) % total,
                        layout,
                    }),
                    Button::Action => {
                        self.input_text.pop();
                        Nav::Stay
                    }
                    Button::Select => match key_at(layout, selected) {
                        Key::Char(c) => {
                            self.input_text.push(c);
                            Nav::Stay
                        }
                        Key::Space => {
                            self.input_text.push(' ');
                            Nav::Stay
                        }
                        Key::Enter => {
                            self.submit_input();
                            Nav::Stay
                        }
                        // Stay on the mode key, so pressing 1 again keeps switching.
                        Key::Mode => {
                            let layout = (layout + 1) % KEY_LAYOUTS.len();
                            Nav::Replace(GUI::TextInput {
                                selected: mode_key_index(layout),
                                layout,
                            })
                        }
                    },
                    Button::Back | Button::Help => Nav::Stay,
                }
            }
        };

        match nav {
            Nav::Stay => {}
            Nav::Replace(gui) => {
                if let Some(top) = self.nav.last_mut() {
                    *top = gui;
                }
            }
            Nav::Push(gui) => {
                self.nav.push(gui);
                self.request_rescan();
            }
        }
    }
}

/// The next song for the Random loop from `bag` (songs not played this
/// round). Never `current`, so nothing plays twice in a row; the song that
/// ended a round is still part of the next one, just not its first pick.
fn shuffle_pick(bag: &mut Vec<usize>, len: usize, current: Option<usize>, fresh: bool) -> Option<usize> {
    if bag.is_empty() {
        // A new round has every song; a fresh start counts the playing
        // one as already played.
        *bag = (0..len).filter(|&j| !fresh || Some(j) != current).collect();
    }
    let choices: Vec<usize> = bag.iter().copied().filter(|&j| Some(j) != current).collect();
    if choices.is_empty() {
        return current.filter(|&c| c < len); // only one song: it repeats
    }
    Some(choices[random_below(choices.len())])
}

/// A random number below `n` (n > 0). Xorshift seeded from the clock:
/// plenty for shuffling songs, no crate needed.
fn random_below(n: usize) -> usize {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        x = nanos | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    (x % n as u64) as usize
}

/// Keep device names short enough for a toast.
fn truncate_name(name: &str) -> String {
    if name.chars().count() > 14 {
        format!("{}...", name.chars().take(13).collect::<String>())
    } else {
        name.to_string()
    }
}

/// Display driver from the command line: `--driver NAME`.
fn driver_override() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let i = args.iter().position(|a| a == "--driver")?;
    args.get(i + 1).cloned()
}

/// Create what bman needs in its folder if it's missing: config.txt (with
/// the defaults), music/, videos/, podcasts/ (+ feeds.txt), audiobooks/.
/// Anything already there is left alone, including a symlink, even a broken
/// one (music/ is often a link to music elsewhere).
fn ensure_layout(config: &Config) {
    if fs::symlink_metadata("config.txt").is_err() {
        save_conf(config);
        info!("Created config.txt with the default settings");
    }
    for dir in [MUSIC_DIR, VIDEOS_DIR, spoken::PODCASTS_DIR, spoken::AUDIOBOOKS_DIR] {
        if fs::symlink_metadata(dir).is_err() {
            match fs::create_dir_all(dir) {
                Ok(()) => info!("Created {dir}/"),
                Err(e) => warn!("Couldn't create {dir}/: {e}"),
            }
        }
    }
    let feeds = Path::new(spoken::PODCASTS_DIR).join(spoken::FEEDS_FILE);
    if Path::new(spoken::PODCASTS_DIR).is_dir() && fs::symlink_metadata(&feeds).is_err() {
        let text = "# Podcasts bman follows: one feed (RSS) URL per line.\n\
                    # Add them here (e.g. from a computer over USB) or with Podcasts > Search podcasts.\n";
        if let Err(e) = fs::write(&feeds, text) {
            warn!("Couldn't create {}: {e}", feeds.display());
        }
    }
}

/// `--terminal` / `-t`: also show the screen in the terminal.
fn terminal_flag() -> bool {
    std::env::args().any(|a| a == "--terminal" || a == "-t")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `bman --autostart ...` / `bman --power ...`: the root helper run through sudo.
    if let Some(code) = root::cli() {
        std::process::exit(code);
    }
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("bman {VERSION}");
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "bman {VERSION}: a one-button music player\n\n\
             Usage: bman [options]   (run it from its folder: music/ and config.txt live there)\n\n\
             \x20 -t, --terminal       also show the screen in this terminal (keys 1-5, arrows, q)\n\
             \x20     --driver NAME    use display NAME this time: whisplay, window, terminal\n\
             \x20 -V, --version        print the version\n\
             \x20 -h, --help           this help\n\n\
             Docs: https://github.com/birdguy-cs/bman/blob/main/DOCS.md"
        );
        return Ok(());
    }

    let config = load_conf();
    ensure_layout(&config);

    let library = Library::scan(MUSIC_DIR).unwrap_or_else(|e| {
        error!("Couldn't read music folder '{MUSIC_DIR}': {e}");
        Library::default()
    });
    for warning in &library.warnings {
        warn!("{warning}");
    }
    info!(
        "Found {} songs: {} singles, {} albums, {} playlists",
        library.song_count(),
        library.singles.len(),
        library.albums.len(),
        library.playlists.len()
    );

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    // The driver in config.txt decides what the screen is (see display_driver.rs).
    // `--driver NAME` overrides it for this run only; `--terminal` mirrors it
    // into the terminal as well (or uses the terminal alone if it can't start).
    let driver = driver_override().unwrap_or_else(|| config.driver.clone());
    let mut display = if terminal_flag() {
        create_display_with_terminal(&driver)?
    } else {
        create_display(&driver)?
    };
    // The battery driver in config.txt decides what the top-right icon shows.
    // An unknown name falls back to the default instead of refusing to start.
    let battery = BatteryMonitor::new(create_battery(&config.battery).unwrap_or_else(|e| {
        warn!("{e}. Using '{DEFAULT_BATTERY_DRIVER}'.");
        create_battery(DEFAULT_BATTERY_DRIVER).expect("default battery driver")
    }));
    let (width, height) = display.size();
    let mut framebuffer = Framebuffer::new(width, height);

    // Real hardware reads the physical button on GPIO. Drivers like the
    // debug window turn mouse clicks into buttons themselves.
    let gpio_buttons = if display.wants_gpio_buttons() {
        match spawn_gpio_buttons(BUTTON_PIN) {
            Ok(rx) => Some(rx),
            Err(e) => {
                warn!("Buttons unavailable: {e}");
                None
            }
        }
    } else {
        None
    };

    let use_system_audio = config.use_system_audio || config.output == Output::Wireless;
    let mut app = App::new(
        library,
        AudioPlayer::new(use_system_audio, config.volume),
        battery,
        config.clone(),
    );

    // Restarted by an update or "Restart bman": carry on with the queue.
    app.resume_after_restart();

    // Bluetooth blocked by rfkill (e.g. a fresh Raspberry Pi OS): unblock it
    // now if sudo needs no password; otherwise it asks when its screen opens.
    app.ensure_unblocked(Radio::Bluetooth, false);
    // USB transfer: set the port up again (configfs is gone after a reboot).
    if app.config.usb {
        app.usb_setup(false);
    }
    // Put the saved system volume back (retries until the sound server is up).
    app.schedule_system_volume(Duration::from_secs(1), 0);

    let frame_time = display.frame_interval();
    let mut display_idle = false;
    let mut display_typing = false;
    let mut display_led = Led::Off;
    let mut display_brightness = app.config.brightness;
    // The part of the dimming the display couldn't do in hardware.
    let mut dim_pct = display.set_brightness(display_brightness);

    while running.load(Ordering::SeqCst) && display.is_open() && !app.restart_due() {
        app.update();

        // Brightness changes apply live while you adjust them in settings.
        // Also dims and fades out before the screen sleeps.
        let brightness = (app.config.brightness as u32 * app.sleep_fade_pct() / 100) as u8;
        if brightness != display_brightness {
            display_brightness = brightness;
            dim_pct = display.set_brightness(display_brightness);
        }

        if app.idle {
            // A black frame keeps the debug window's events flowing; the
            // Whisplay skips frames that haven't changed.
            framebuffer.clear(0xFF000000);
            if let Err(e) = display.present(&framebuffer) {
                error!("Display error: {e}");
                break;
            }
        } else {
            let video_frame = app.video_frame();
            let now_playing = app.now_playing();
            let devices = app.bluetooth.devices();
            let bt_status = app.bluetooth.status();
            let bt_connected = devices
                .iter()
                .find(|d| d.connected)
                .map(|d| d.name.clone());
            let (input_title, input_subtitle, input_ok) = app.input_labels();
            let networks = app.network.networks();
            let net_status = app.network.status();
            let net_ip = app.network.ip();
            let net_connected = app.network.connected_ssid();
            let update_value = app.system_value();
            let activity = app.activity();
            let (system_rows, system_status) = app.system_rows();
            let update_tag = app.offered_tag();
            let (yt_status, yt_rows) = app.yt_rows();
            let list = match app.gui() {
                GUI::StreamResults { list, .. } => *list,
                _ => StreamList::Results,
            };
            let (yt_list_title, yt_list_status, yt_list) = app.stream_list_view(list);
            let (spoken_title, spoken_status, spoken_rows) = app.spoken_view();

            let view = View {
                gui: app.gui(),
                library: &app.library,
                queue: &app.queue,
                current: app.current,
                now_playing: now_playing.as_ref(),
                art: app.art.as_ref(),
                video: video_frame.as_deref(),
                bt_devices: &devices,
                bt_status: &bt_status,
                bt_busy: app.bluetooth.is_busy(),
                bt_connected: bt_connected.as_deref(),
                net_networks: &networks,
                net_status: &net_status,
                net_busy: app.network.is_busy(),
                net_enabled: app.network.wifi_enabled(),
                net_ip: net_ip.as_deref(),
                net_connected: net_connected.as_deref(),
                input_title: &input_title,
                input_subtitle: &input_subtitle,
                input_ok: input_ok,
                input_text: &app.input_text,
                input_masked: matches!(app.input_purpose, Some(InputPurpose::SudoPassword { .. })),
                target: app.target,
                output: app.config.output,
                battery_driver: &app.config.battery,
                system_volume: app.system_volume_now,
                activity: activity.as_deref(),
                update_value: &update_value,
                system_rows: &system_rows,
                system_status: &system_status,
                update_tag: &update_tag,
                yt_status: &yt_status,
                yt_rows: &yt_rows,
                yt_list_title,
                yt_list_status: &yt_list_status,
                yt_list: &yt_list,
                yt_to_liked: app.stream_to_liked,
                spoken_title: &spoken_title,
                spoken_status: &spoken_status,
                spoken_rows: &spoken_rows,
                toast: app.active_toast(),
                help: app.help,
                style: Style {
                    accent: app.config.accent,
                    time_ms: app.anim_ms(),
                    volume_pct: app.volume_pct(),
                    timeout_secs: app.config.screen_timeout,
                    brightness_pct: app.config.brightness,
                    dim_pct,
                    battery: app.battery.get(),
                    header_status: activity.is_some(),
                },
            };

            if let Err(e) = display_frame(display.as_mut(), &mut framebuffer, &view) {
                error!("Display error: {e}");
                break;
            }
        }

        // Backlight off after the black frame above, on after the first real frame.
        if app.idle != display_idle {
            display.set_idle(app.idle);
            display_idle = app.idle;
        }

        // Status LED (Whisplay): only written when the colour changes.
        let led = app.led();
        if led != display_led {
            display.set_led(led);
            display_led = led;
        }


        // A real keyboard types on the keyboard screen (window / terminal).
        let typing = matches!(app.gui(), GUI::TextInput { .. });
        if typing != display_typing {
            display.set_typing(typing);
            display_typing = typing;
        }

        let mut buttons = display.poll_buttons();
        if let Some(rx) = &gpio_buttons {
            buttons.extend(rx.try_iter());
        }
        for button in buttons {
            app.handle_button(button);
        }
        for key in display.poll_typed() {
            app.handle_typed(key);
        }

        thread::sleep(if app.fading() { frame_time.min(FADE_FRAME) } else { frame_time });
    }

    // Where the podcast / audiobook was (before the player lets go of it).
    app.save_spoken_position();
    app.player.stop();
    app.youtube.purge_temp();
    // Shutting the device down: leave it red until the power goes.
    if !app.powering_off {
        display.set_led(Led::Off);
    }

    eprintln!();
    info!("Shutting down, saving config...");
    save_conf(&app.config);

    if app.restart_due() {
        // Let go of the screen, GPIO and audio before the new binary takes them.
        drop(display);
        drop(gpio_buttons);
        drop(app);
        info!("Restarting into the new version...");
        let e = update::restart();
        error!("{e}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffle_never_repeats() {
        for len in [1, 2, 3, 7] {
            let mut bag = Vec::new();
            let mut current = Some(0);
            let mut played = vec![0];
            let mut fresh = true;
            for _ in 0..len * 50 - 1 {
                bag.retain(|&j| Some(j) != current);
                let next = shuffle_pick(&mut bag, len, current, fresh).unwrap();
                fresh = false;
                if len > 1 {
                    assert_ne!(Some(next), current, "same song twice in a row");
                }
                current = Some(next);
                played.push(next);
            }
            // Every block of `len` songs is one full round (song 0 started
            // the first one).
            if len > 1 {
                for round in played.chunks_exact(len) {
                    let mut r = round.to_vec();
                    r.sort();
                    assert_eq!(r, (0..len).collect::<Vec<_>>());
                }
            }
        }
    }

    #[test]
        fn sleep_steps_down_to_never() {
        assert_eq!(step_timeout(45, false), 30);
        assert_eq!(step_timeout(30, false), 0); // below 30: never
        assert_eq!(step_timeout(0, false), 0);
        assert_eq!(step_timeout(0, true), 30);
        assert_eq!(step_timeout(300, true), 300);
    }
}

/// 75 -> "1:15", 3700 -> "1:01:40".
fn clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}
