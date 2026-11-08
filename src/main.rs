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
mod root;
mod sched;
mod terminal;
mod update;
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
use crate::update::*;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
    Arc,
};
use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

/// Folder scanned for music, relative to where the program is run.
const MUSIC_DIR: &str = "music";

/// Minimum time between library rescans (they're asked for on every screen
/// change; files copied in still show up within this long).
const RESCAN_INTERVAL: Duration = Duration::from_secs(5);

/// "Previous" past this point in a song restarts it instead.
const RESTART_THRESHOLD: Duration = Duration::from_secs(3);

/// How long a toast message stays on screen.
const TOAST_DURATION: Duration = Duration::from_millis(1500);
/// Errors stay up longer so they can be read.
const ERROR_TOAST_DURATION: Duration = Duration::from_secs(4);
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
}

/// System screen rows.
const SYSTEM_ROWS: usize = 6;
const ROW_RESTART: usize = 3;
const ROW_SHUTDOWN: usize = 4;
const ROW_RESTART_APP: usize = 5;
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
    /// An update was installed: leave the main loop at this time and restart
    /// into the new binary (the delay lets the toast show).
    restart_at: Option<Instant>,
}

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
            restart_at: None,
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
            loop_mode: self.loop_mode,
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
        self.play_index(index);
    }

    fn play_index(&mut self, index: usize) {
        let Some(song) = self.queue.get(index) else {
            self.current = None;
            return;
        };
        let path = song.path.clone();

        self.current = Some(index);
        self.reset_anim();
        self.player.play(path.clone());

        // Load cover art in the background; the circle shows until it's ready.
        self.art = None;
        self.art_rx = Some(spawn_art_loader(path));
    }

    /// Skip to the next track in the queue (wraps to the start).
    fn skip_next(&mut self) {
        if self.queue.is_empty() {
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

        let next = match self.loop_mode {
            LoopMode::Track => Some(i),
            LoopMode::Queue => Some((i + 1) % len.max(1)),
            LoopMode::Off if i + 1 < len => Some(i + 1),
            LoopMode::Off => None,
        };

        match next {
            Some(n) => self.play_index(n),
            None => {
                self.current = None;
                self.art = None;
            }
        }
    }

    fn remove_from_queue(&mut self, index: usize) {
        if index >= self.queue.len() {
            return;
        }
        self.queue.remove(index);

        match self.current {
            // Removed the playing song: the next one slides into its slot.
            Some(c) if c == index => {
                if index < self.queue.len() {
                    self.play_index(index);
                } else if self.loop_mode == LoopMode::Queue && !self.queue.is_empty() {
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
        match self.library.create_playlist_with(self.queue.clone()) {
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
                self.queue.push(song);
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
                UpdateEvent::Failed(e) => self.show_toast_for(e, ERROR_TOAST_DURATION),
                UpdateEvent::Installed(tag) => {
                    self.show_toast(format!("Restarting into {tag}"));
                    self.restart_at = Some(Instant::now() + Duration::from_millis(1500));
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
            UpdateState::Available(r) => format!("Install {}", r.tag),
            UpdateState::Installing => "Installing...".into(),
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
        let status = match self.init {
            Some(init) => format!("bman v{VERSION} ({})", init.name()),
            None => format!("bman v{VERSION}"),
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
            UpdateState::Checking | UpdateState::Installing | UpdateState::Installed => {}
            _ => self.updater.check(true),
        }
    }

    fn toggle_boot(&mut self) {
        if self.root_job.is_some() {
            return;
        }
        if self.init.is_none() {
            self.show_toast_for("Unknown init system", ERROR_TOAST_DURATION);
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
            Err(e) => self.show_toast_for(e, ERROR_TOAST_DURATION),
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
                    self.show_toast_for(msg, ERROR_TOAST_DURATION);
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
                if let RootJob::Unblock { radio, .. } = job {
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
            Err(RootError::NeedsPassword) => {
                let gui = self.open_keyboard(InputPurpose::SudoPassword { job });
                self.nav.push(gui);
            }
            Err(RootError::WrongPassword) => {
                self.show_toast_for("Wrong password", ERROR_TOAST_DURATION);
                let gui = self.open_keyboard(InputPurpose::SudoPassword { job });
                self.nav.push(gui);
            }
            Err(RootError::Failed(e)) => {
                warn!("{e}");
                self.show_toast_for(e, ERROR_TOAST_DURATION);
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
            || matches!(state, UpdateState::Installing | UpdateState::Installed)
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

    /// Call once per frame.
    fn update(&mut self) {
        self.poll_scan();
        self.check_battery();
        self.handle_bluetooth();
        self.handle_updates();
        self.poll_root_job();
        self.poll_system_volume();
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
            Some(InputPurpose::SudoPassword { job }) => {
                let what = match job {
                    RootJob::Boot(_) => "start on boot",
                    RootJob::Power(PowerAction::Reboot) => "restart",
                    RootJob::Power(PowerAction::PowerOff) => "shut down",
                    RootJob::Unblock { radio: Radio::Bluetooth, .. } => "unblock Bluetooth",
                    RootJob::Unblock { radio: Radio::Wifi, .. } => "unblock Wi-Fi",
                };
                ("Password".into(), format!("sudo password ({what})"), "continue")
            }
            None => ("Type".into(), String::new(), "finish"),
        }
    }

    /// "OK" on the keyboard.
    fn submit_input(&mut self) {
        match self.input_purpose.clone() {
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
        self.request_rescan();
    }

    // --- input --------------------------------------------------------

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
                        0 => GUI::Singles { selected: 0 },
                        1 => GUI::Albums { selected: 0 },
                        2 => GUI::Artists { selected: 0 },
                        3 => GUI::Playlists { selected: 0 },
                        // Row 0 is "Save as playlist"; start on the playing song.
                        4 => GUI::Queue {
                            selected: self.current.map_or(0, |c| c + 1),
                        },
                        5 => GUI::NowPlaying,
                        _ => GUI::Settings {
                            selected: 0,
                            editing: 0,
                        },
                    })
                } else {
                    Nav::Stay
                }
            }

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

            GUI::NowPlaying => {
                match button {
                    Button::Select => self.player.toggle_pause(),
                    Button::Down => self.skip_next(),
                    Button::Up => self.skip_previous(),
                    Button::Action => {
                        self.loop_mode = self.loop_mode.next();
                        self.show_toast(format!("Loop: {}", self.loop_mode.label()));
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
                            self.restart_at = Some(Instant::now() + Duration::from_millis(800));
                        }
                        _ => {}
                    }
                    Nav::Stay
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

/// `--terminal` / `-t`: also show the screen in the terminal.
fn terminal_flag() -> bool {
    std::env::args().any(|a| a == "--terminal" || a == "-t")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `bman --autostart ...` / `bman --power ...`: the root helper run through sudo.
    if let Some(code) = root::cli() {
        std::process::exit(code);
    }

    let config = load_conf();

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

    // Bluetooth blocked by rfkill (e.g. a fresh Raspberry Pi OS): unblock it
    // now if sudo needs no password; otherwise it asks when its screen opens.
    app.ensure_unblocked(Radio::Bluetooth, false);
    // Put the saved system volume back (retries until the sound server is up).
    app.schedule_system_volume(Duration::from_secs(1), 0);

    let frame_time = display.frame_interval();
    let mut display_idle = false;
    let mut display_brightness = app.config.brightness;
    // The part of the dimming the display couldn't do in hardware.
    let mut dim_pct = display.set_brightness(display_brightness);

    while running.load(Ordering::SeqCst) && display.is_open() && !app.restart_due() {
        app.update();

        if app.idle {
            // A black frame keeps the debug window's events flowing; the
            // Whisplay skips frames that haven't changed.
            framebuffer.clear(0xFF000000);
            if let Err(e) = display.present(&framebuffer) {
                error!("Display error: {e}");
                break;
            }
        } else {
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
            let (system_rows, system_status) = app.system_rows();
            let update_tag = app.offered_tag();

            let view = View {
                gui: app.gui(),
                library: &app.library,
                queue: &app.queue,
                current: app.current,
                now_playing: now_playing.as_ref(),
                art: app.art.as_ref(),
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
                update_value: &update_value,
                system_rows: &system_rows,
                system_status: &system_status,
                update_tag: &update_tag,
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

        // Brightness changes apply live while you adjust them in settings.
        if app.config.brightness != display_brightness {
            display_brightness = app.config.brightness;
            dim_pct = display.set_brightness(display_brightness);
        }

        let mut buttons = display.poll_buttons();
        if let Some(rx) = &gpio_buttons {
            buttons.extend(rx.try_iter());
        }
        for button in buttons {
            app.handle_button(button);
        }

        thread::sleep(frame_time);
    }

    app.player.stop();

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
    fn sleep_steps_down_to_never() {
        assert_eq!(step_timeout(45, false), 30);
        assert_eq!(step_timeout(30, false), 0); // below 30: never
        assert_eq!(step_timeout(0, false), 0);
        assert_eq!(step_timeout(0, true), 30);
        assert_eq!(step_timeout(300, true), 300);
    }
}
