use crate::battery::Battery;
use crate::art::Art;
use crate::audio::LoopMode;
use crate::bluetooth::BtDevice;
use crate::network::WifiNetwork;
use crate::buttons::{Button, Typed};
use crate::config::Output;
use crate::library::{Library, Song};
use font8x8::legacy::BASIC_LEGACY;
use std::time::Duration;

const BG: u32 = 0xFF000000;
const WHITE: u32 = 0xFFFFFFFF;
const BLACK: u32 = 0xFF000000;
const GRAY: u32 = 0xFF9A9A9A;
const DARK_GRAY: u32 = 0xFF303030;

/// Scrolling text: pixels per second, and the pause at each end.
const SCROLL_SPEED: u64 = 40;


const SCROLL_PAUSE_MS: u64 = 1200;

/// Rows on the settings screen: Volume, Accent, Audio, Bright, Sleep,
/// Battery, Network, Bluetooth, System.
pub const SETTINGS_ROWS: usize = 9;

/// Keyboard layouts (name, characters), switched with the mode key.
pub const KEY_LAYOUTS: [(&str, &str); 4] = [
    ("abc", "abcdefghijklmnopqrstuvwxyz"),
    ("ABC", "ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
    ("123", "1234567890"),
    ("#+=", "!@#$%^&*()-_=+[]{};:'\",.<>/?\\|`~"),
];

/// A key on the on-screen keyboard. Each layout is its characters, then
/// Space, Enter, Mode (so Space sits left of Enter and Mode right of it,
/// and Mode wraps round to the first character).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Space,
    Enter,
    /// Switch to the next layout.
    Mode,
}

fn layout_chars(layout: usize) -> &'static str {
    KEY_LAYOUTS[layout % KEY_LAYOUTS.len()].1
}

pub fn key_count(layout: usize) -> usize {
    layout_chars(layout).chars().count() + 3
}

pub fn key_at(layout: usize, i: usize) -> Key {
    let chars = layout_chars(layout);
    let n = chars.chars().count();
    match i.checked_sub(n) {
        None => Key::Char(chars.chars().nth(i).unwrap_or(' ')),
        Some(0) => Key::Space,
        Some(1) => Key::Enter,
        Some(_) => Key::Mode,
    }
}

/// Index of the Mode key in a layout (where the selection stays after a switch).
pub fn mode_key_index(layout: usize) -> usize {
    key_count(layout) - 1
}

pub const MENU_ITEMS: [&str; 8] = [
    "Singles",
    "Albums",
    "Artists",
    "Playlists",
    "Queue",
    "Now Playing",
    "Streaming",
    "Settings",
];

/// Streaming services, in the order the Streaming screen lists them.
pub const STREAMING_SERVICES: [&str; 1] = ["YouTube Music"];

pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

/// Per-frame look and animation info.
#[derive(Clone, Copy)]
pub struct Style {
    /// Main UI colour (0xFFRRGGBB).
    pub accent: u32,
    /// Milliseconds since the last button press / track change. Long text
    /// scrolls based on this, so it starts from the beginning after input.
    pub time_ms: u64,
    /// Volume 0-100, shown on the settings screen.
    pub volume_pct: u8,
    /// Seconds of inactivity before the screen goes idle, shown in settings.
    pub timeout_secs: u32,
    /// Backlight brightness 10-100, shown in settings.
    pub brightness_pct: u8,
    /// How much of the dimming the display couldn't do in hardware, so the
    /// frame's colours are scaled by this percentage (100 = no scaling).
    pub dim_pct: u8,
    /// Battery shown at the top right.
    pub battery: Battery,
    /// An activity line is shown under the title, so titles sit higher.
    pub header_status: bool,
}

/// A display driver. Implement this (and add it to create_display in
/// display_driver.rs) to support new hardware.
pub trait Display {
    /// Framebuffer size in pixels. The layout is designed for 240x280.
    fn size(&self) -> (u32, u32);

    fn present(
        &mut self,
        framebuffer: &Framebuffer,
    ) -> Result<(), Box<dyn std::error::Error>>;

    /// Buttons this display can produce itself (the debug window turns mouse
    /// clicks into buttons). Real hardware buttons come from buttons.rs.
    fn poll_buttons(&mut self) -> Vec<Button> {
        Vec::new()
    }

    /// Tells a driver with a real keyboard that the on-screen keyboard is
    /// open, so printable keys type text instead of acting as the button.
    fn set_typing(&mut self, _typing: bool) {}

    /// Text keys typed since the last call (only while typing). Called right
    /// after `poll_buttons`.
    fn poll_typed(&mut self) -> Vec<Typed> {
        Vec::new()
    }

    /// False once the user has closed the window.
    fn is_open(&self) -> bool {
        true
    }

    /// Whether the physical GPIO button should be read with this driver.
    fn wants_gpio_buttons(&self) -> bool {
        true
    }

    /// Set the screen brightness (10-100). A driver with a dimmable backlight
    /// applies as much as it can in hardware and returns the percentage that
    /// is still left to do by scaling the picture's colours (100 = none).
    /// The default can't dim anything, so all of it is done in software.
    fn set_brightness(&mut self, percent: u8) -> u8 {
        percent
    }

    /// Called when the screen goes idle (true) or wakes up (false). Real
    /// hardware turns its backlight off; windows don't need to do anything.
    fn set_idle(&mut self, _idle: bool) {}

    /// Time between frames.
    fn frame_interval(&self) -> Duration {
        Duration::from_millis(100)
    }
}

/// Which screen is showing. Screens are kept on a stack in main so Back
/// returns to wherever you came from. `selected` is the highlighted row.
#[derive(Clone)]
pub enum GUI {
    Menu { selected: usize },
    Singles { selected: usize },
    Albums { selected: usize },
    Album { album: usize, selected: usize },
    Artists { selected: usize },
    /// An artist's albums first, then their singles.
    Artist { artist: usize, selected: usize },
    /// Row 0 is "+ New playlist", playlist N is row N + 1.
    Playlists { selected: usize },
    Playlist { playlist: usize, selected: usize },
    Queue { selected: usize },
    NowPlaying,
    /// Rows: 0 volume, 1 accent, 2 audio output, 3 bluetooth.
    /// `editing`: 0 = browsing rows, otherwise the field being changed
    /// (volume: 1; accent: 1 = red, 2 = green, 3 = blue).
    Settings { selected: usize, editing: u8 },
    /// Row 0 is "Scan", devices are rows 1...
    Bluetooth { selected: usize },
    /// Rows: 0 Wi-Fi on/off, 1 IP address, 2 scan, networks from row 3.
    Network { selected: usize },
    /// On-screen keyboard (Wi-Fi password, playlist name). `selected` is
    /// the picker index (0 = OK). What it's for is kept in the App.
    TextInput { selected: usize, layout: usize },
    /// Rows: 0 music volume, 1 system volume. `editing`: changing the row.
    Volume { selected: usize, editing: bool },
    /// Rows: 0 check / install update, 1 auto check on/off, 2 start on boot,
    /// 3 restart device, 4 shut down, 5 restart bman.
    System { selected: usize },
    /// "Update available": 0 update now, 1 not now.
    UpdatePrompt { selected: usize },
    /// Streaming services (STREAMING_SERVICES).
    Streaming { selected: usize },
    /// YouTube Music. Rows: 0 search, 1 last results, 2 liked songs,
    /// 3 YouTube likes, 4 set up / update.
    YouTube { selected: usize },
    /// A list of YouTube Music songs.
    StreamResults { list: StreamList, selected: usize },
}

/// Which songs a `GUI::StreamResults` screen shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StreamList {
    /// The last search.
    Results,
    /// bman's own liked songs.
    Liked,
    /// The YouTube account's liked songs (needs cookies).
    Account,
}

/// Everything the now-playing screen needs.
pub struct NowPlayingInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub length: u64,
    pub time: u64,
    pub paused: bool,
    /// A streamed song still downloading.
    pub loading: bool,
    pub loop_mode: LoopMode,
}

/// Everything needed to draw one frame.
pub struct View<'a> {
    pub gui: &'a GUI,
    pub library: &'a Library,
    pub queue: &'a [Song],
    /// Index into `queue` of the song being played.
    pub current: Option<usize>,
    pub now_playing: Option<&'a NowPlayingInfo>,
    pub art: Option<&'a Art>,
    pub bt_devices: &'a [BtDevice],
    pub bt_status: &'a str,
    pub bt_busy: bool,
    pub bt_connected: Option<&'a str>,
    pub net_networks: &'a [WifiNetwork],
    pub net_status: &'a str,
    pub net_busy: bool,
    pub net_enabled: bool,
    pub net_ip: Option<&'a str>,
    pub net_connected: Option<&'a str>,
    /// Keyboard screen: header, subtitle, what OK does, text typed so far.
    pub input_title: &'a str,
    pub input_subtitle: &'a str,
    pub input_ok: &'a str,
    pub input_text: &'a str,
    /// Hide all but the last typed character.
    pub input_masked: bool,
    /// Playlist that 5 presses add to. None = the queue.
    pub target: Option<usize>,
    pub output: Output,
    /// Battery driver name from config, shown in settings.
    pub battery_driver: &'a str,
    /// What the device is doing (song playing, update installing...), shown
    /// in small text under every screen's title. None = nothing to show.
    pub activity: Option<&'a str>,
    /// System (output) volume in percent; None = no usable mixer.
    pub system_volume: Option<u8>,
    /// Settings "System" row value (version, or "update!").
    pub update_value: &'a str,
    /// System screen rows and status line.
    pub system_rows: &'a [String],
    pub system_status: &'a str,
    /// Version offered by the update prompt.
    pub update_tag: &'a str,
    /// YouTube Music: service status, its screen's rows, and the song list
    /// on screen (title, status line, songs).
    pub yt_status: &'a str,
    pub yt_rows: &'a [String],
    pub yt_list_title: &'a str,
    pub yt_list_status: &'a str,
    pub yt_list: &'a [String],
    /// 5 on a streamed song likes it instead of queueing it.
    pub yt_to_liked: bool,
    pub toast: Option<&'a str>,
    /// Show the help overlay (long press).
    pub help: bool,
    pub style: Style,
}

impl Framebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![BG; (width * height) as usize],
        }
    }

    pub fn clear(&mut self, color: u32) {
        self.pixels.fill(color);
    }
}

pub fn display_frame(
    display: &mut dyn Display,
    framebuffer: &mut Framebuffer,
    view: &View<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    framebuffer.clear(BG);
    render_framebuffer(framebuffer, view);
    dim(framebuffer, view.style.dim_pct);
    display.present(framebuffer)?;

    Ok(())
}

/// Screen brightness: scale every pixel's colour by `percent` (10-100).
/// The framebuffer is redrawn from scratch each frame, so this never stacks.
fn dim(fb: &mut Framebuffer, percent: u8) {
    if percent >= 100 {
        return;
    }

    let scale = percent.clamp(10, 100) as u32;
    for pixel in fb.pixels.iter_mut() {
        let r = ((*pixel >> 16) & 0xFF) * scale / 100;
        let g = ((*pixel >> 8) & 0xFF) * scale / 100;
        let b = (*pixel & 0xFF) * scale / 100;
        *pixel = 0xFF000000 | (r << 16) | (g << 8) | b;
    }
}

fn target_name<'v>(view: &'v View<'_>) -> &'v str {
    view.target
        .and_then(|p| view.library.playlists.get(p))
        .map(|p| p.name.as_str())
        .unwrap_or("queue")
}

pub fn render_framebuffer(fb: &mut Framebuffer, view: &View<'_>) {
    if view.help {
        draw_help(fb, view);
        draw_activity(fb, view);
        draw_battery(fb, view.style);
        return;
    }

    let style = view.style;
    let library = view.library;


    match view.gui {
        GUI::Menu { selected } => {
            draw_list(fb, "BMan", None, MENU_ITEMS.len(), *selected, &|i| {
                MENU_ITEMS[i].to_string()
            }, style)
        }

        GUI::Singles { selected } => {
            let hint = format!("5x: add to {}", target_name(view));
            draw_list(fb, "Singles", Some(&hint), library.singles.len(), *selected, &|i| {
                library.singles[i].label()
            }, style)
        }

        GUI::Albums { selected } => {
            draw_list(fb, "Albums", None, library.albums.len(), *selected, &|i| {
                library.albums[i].label()
            }, style)
        }

        GUI::Album { album, selected } => match library.albums.get(*album) {
            Some(a) => {
                let artist = a.artist.as_deref().unwrap_or("Unknown Artist");
                draw_list(fb, &a.name, Some(artist), a.songs.len(), *selected, &|i| {
                    a.songs[i].album_label()
                }, style);
            }
            None => draw_list(fb, "Album", None, 0, 0, &|_| String::new(), style),
        },

        GUI::Artists { selected } => {
            draw_list(fb, "Artists", None, library.artists.len(), *selected, &|i| {
                library.artists[i].name.clone()
            }, style)
        }

        GUI::Artist { artist, selected } => match library.artists.get(*artist) {
            Some(a) => {
                let album_count = a.albums.len();
                let hint = format!("{} albums, {} singles", album_count, a.singles.len());
                draw_list(fb, &a.name, Some(&hint), album_count + a.singles.len(), *selected, &|i| {
                    if i < album_count {
                        library.albums[a.albums[i]].name.clone()
                    } else {
                        format!("- {}", library.singles[a.singles[i - album_count]].title)
                    }
                }, style);
            }
            None => draw_list(fb, "Artist", None, 0, 0, &|_| String::new(), style),
        },

        GUI::Playlists { selected } => {
            let hint = format!("5x: add songs to this ({})", target_name(view));
            draw_list(fb, "Playlists", Some(&hint), library.playlists.len() + 1, *selected, &|i| {
                if i == 0 {
                    "+ New playlist".to_string()
                } else {
                    let name = &library.playlists[i - 1].name;
                    if view.target == Some(i - 1) {
                        format!("* {name}")
                    } else {
                        name.clone()
                    }
                }
            }, style)
        }

        GUI::Playlist { playlist, selected } => match library.playlists.get(*playlist) {
            Some(p) => {
                // Row 0 renames, songs start at row 1.
                draw_list(fb, &p.name, Some("5x: remove song"), p.songs.len() + 1, *selected, &|i| {
                    if i == 0 {
                        "Rename playlist".to_string()
                    } else {
                        p.songs[i - 1].label()
                    }
                }, style);
            }
            None => draw_list(fb, "Playlist", None, 0, 0, &|_| String::new(), style),
        },

        GUI::Queue { selected } => {
            // Row 0 saves the queue as a playlist, songs start at row 1.
            draw_list(fb, "Queue", Some("5x: remove song"), view.queue.len() + 1, *selected, &|i| {
                if i == 0 {
                    return "Save as playlist".to_string();
                }
                let marker = if view.current == Some(i - 1) { "> " } else { "  " };
                format!("{marker}{}", view.queue[i - 1].label())
            }, style)
        }

        GUI::NowPlaying => draw_now_playing(fb, view),

        GUI::Settings { selected, editing } => draw_settings(fb, *selected, *editing, view),

        GUI::Bluetooth { selected } => {
            let status = if view.bt_status.is_empty() {
                None
            } else {
                Some(view.bt_status)
            };
            draw_list(fb, "Bluetooth", status, view.bt_devices.len() + 1, *selected, &|i| {
                if i == 0 {
                    if view.bt_busy {
                        "Working...".to_string()
                    } else {
                        "Scan for devices".to_string()
                    }
                } else {
                    let d = &view.bt_devices[i - 1];
                    let marker = if d.connected {
                        "* "
                    } else if d.paired {
                        "+ "
                    } else {
                        "  "
                    };
                    format!("{marker}{}", d.name)
                }
            }, style)
        }

        GUI::Network { selected } => {
            let status = if view.net_status.is_empty() {
                None
            } else {
                Some(view.net_status)
            };
            let count = if view.net_enabled { view.net_networks.len() + 3 } else { 2 };
            draw_list(fb, "Network", status, count, *selected, &|i| match i {
                0 => format!("Wi-Fi: {}", if view.net_enabled { "On" } else { "Off" }),
                1 => format!("IP {}", view.net_ip.unwrap_or("none")),
                2 if view.net_busy => "Working...".to_string(),
                2 => "Scan for networks".to_string(),
                _ => {
                    let n = &view.net_networks[i - 3];
                    let marker = if n.connected {
                        "* "
                    } else if n.saved {
                        "+ "
                    } else {
                        "  "
                    };
                    let lock = if n.secured { "" } else { " (open)" };
                    format!("{marker}{} {}%{lock}", n.ssid, n.strength)
                }
            }, style)
        }

        GUI::TextInput { selected, layout } => draw_text_input(fb, *selected, *layout, view),

        GUI::Volume { selected, editing } => draw_volume(fb, *selected, *editing, view),

        GUI::System { selected } => {
            let status = Some(view.system_status).filter(|s| !s.is_empty());
            draw_list(fb, "System", status, view.system_rows.len(), *selected, &|i| {
                view.system_rows[i].clone()
            }, style)
        }

        GUI::UpdatePrompt { selected } => {
            let status = format!("{} is out", view.update_tag);
            draw_list(fb, "Update?", Some(&status), 2, *selected, &|i| {
                if i == 0 { "Update now".into() } else { "Not now".into() }
            }, style)
        }

        GUI::Streaming { selected } => {
            draw_list(fb, "Streaming", None, STREAMING_SERVICES.len(), *selected, &|i| {
                STREAMING_SERVICES[i].to_string()
            }, style)
        }

        GUI::YouTube { selected } => {
            let status = Some(view.yt_status).filter(|s| !s.is_empty());
            draw_list(fb, "YouTube Music", status, view.yt_rows.len(), *selected, &|i| {
                view.yt_rows[i].clone()
            }, style)
        }

        GUI::StreamResults { selected, .. } => {
            let status = Some(view.yt_list_status).filter(|s| !s.is_empty());
            draw_list(fb, view.yt_list_title, status, view.yt_list.len(), *selected, &|i| {
                view.yt_list[i].clone()
            }, style)
        }
    }

    draw_activity(fb, view);
    draw_battery(fb, style);

    if let Some(text) = view.toast {
        draw_toast(fb, text, style);
    }
}

/// Title baseline: a little higher when the activity line is under it.
fn title_y(style: Style) -> i32 {
    if style.header_status { 2 } else { 6 }
}

/// The activity line in the header (left of the battery), scrolling if long.
fn draw_activity(fb: &mut Framebuffer, view: &View<'_>) {
    let Some(text) = view.activity else { return };
    let style = view.style;
    let w = fb.width as i32;
    draw_scrolling(fb, 8, 19, text, 1, text_on(style.accent), w - 16 - header_reserved(style), false, style.time_ms);
}

// ---------------------------------------------------------------------------
// Screens
// ---------------------------------------------------------------------------

/// A titled list with the selected row kept in the middle of the screen.
/// Long header text and the selected row scroll; other rows are cut with "..".
fn draw_list(
    fb: &mut Framebuffer,
    title: &str,
    subtitle: Option<&str>,
    count: usize,
    selected: usize,
    label: &dyn Fn(usize) -> String,
    style: Style,
) {
    const ROW_H: i32 = 24;

    let w = fb.width as i32;
    let h = fb.height as i32;
    let header_h = if subtitle.is_some() { 42 } else { 28 };
    let header_text = text_on(style.accent);

    // Header
    fill_rect(fb, 0, 0, w, header_h, style.accent);
    draw_scrolling(fb, 8, title_y(style), title, 2, header_text, w - 16 - header_reserved(style), false, style.time_ms);
    if let Some(sub) = subtitle {
        draw_scrolling(fb, 8, 30, sub, 1, header_text, w - 16, false, style.time_ms);
    }

    if count == 0 {
        draw_centered_text(fb, h / 2, "(empty)", 2, GRAY);
        return;
    }

    // Odd number of rows so there is a true middle row, centered vertically.
    let mut visible = (h - header_h) / ROW_H;
    if visible % 2 == 0 {
        visible -= 1;
    }
    let middle = visible / 2;
    let list_top = header_h + (h - header_h - visible * ROW_H) / 2;

    for i in 0..visible {
        let index = selected as i64 + (i - middle) as i64;
        if index < 0 || index >= count as i64 {
            continue;
        }

        let y = list_top + i * ROW_H;
        let is_selected = i == middle;
        let text = label(index as usize);

        if is_selected {
            fill_rect(fb, 0, y, w, ROW_H, DARK_GRAY);
            fill_rect(fb, 0, y, 4, ROW_H, style.accent);
            draw_scrolling(fb, 12, y + 4, &text, 2, WHITE, w - 16, false, style.time_ms);
        } else {
            draw_text(fb, 12, y + 4, &truncate(&text, 14), 2, GRAY);
        }
    }
}

fn draw_now_playing(fb: &mut Framebuffer, view: &View<'_>) {
    let style = view.style;
    let w = fb.width as i32;

    // Header bar like every other screen.
    fill_rect(fb, 0, 0, w, 28, style.accent);
    // Same as list headers: scrolls if it would run into the battery.
    draw_scrolling(fb, 8, title_y(style), "Now Playing", 2, text_on(style.accent), w - 16 - header_reserved(style), false, style.time_ms);

    let Some(info) = view.now_playing else {
        draw_centered_text(fb, 132, "Nothing", 2, GRAY);
        draw_centered_text(fb, 156, "playing", 2, GRAY);
        return;
    };

    // Cover art, or a placeholder while it loads / if there is none.
    match view.art {
        Some(art) => draw_art(fb, art, (w - art.size as i32) / 2, 32),
        None => {
            draw_circle(fb, w / 2, 80, 36, DARK_GRAY);
            draw_circle(fb, w / 2, 80, 8, style.accent);
        }
    }

    // Text: centered when it fits, scrolling when it doesn't.
    let max_w = w - 16;
    draw_scrolling(fb, 8, 134, &info.title, 2, WHITE, max_w, true, style.time_ms);
    draw_scrolling(fb, 8, 158, &info.artist, 1, GRAY, max_w, true, style.time_ms);
    draw_scrolling(fb, 8, 171, &info.album, 1, GRAY, max_w, true, style.time_ms);
    draw_centered_text(
        fb,
        188,
        if info.loading {
            "Loading..."
        } else if info.paused {
            "Paused"
        } else {
            "Playing"
        },
        1,
        WHITE,
    );

    // Progress bar
    let bar_x = 20;
    let bar_y = 208;
    let bar_w = w - 40;
    let bar_h = 8;

    fill_rect(fb, bar_x, bar_y, bar_w, bar_h, DARK_GRAY);

    let progress = if info.length > 0 {
        (info.time.min(info.length) as f64 / info.length as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    fill_rect(
        fb,
        bar_x,
        bar_y,
        (bar_w as f64 * progress) as i32,
        bar_h,
        style.accent,
    );

    // Times
    draw_text(fb, bar_x, bar_y + 14, &format_time(info.time), 1, GRAY);
    let total = if info.length > 0 {
        format_time(info.length)
    } else {
        "--:--".to_string()
    };
    let total_w = text_width(&total, 1);
    draw_text(fb, bar_x + bar_w - total_w, bar_y + 14, &total, 1, GRAY);

    // Loop mode
    draw_centered_text(
        fb,
        250,
        &format!("Loop: {}", info.loop_mode.label()),
        1,
        GRAY,
    );
}

fn row_background(fb: &mut Framebuffer, y: i32, h: i32, selected: bool, accent: u32) {
    if selected {
        let w = fb.width as i32;
        fill_rect(fb, 0, y, w, h, DARK_GRAY);
        fill_rect(fb, 0, y, 4, h, accent);
    }
}

/// Settings: volume, accent colour, audio output, brightness, screen timeout,
/// battery driver, network, Bluetooth and updates.
fn draw_settings(fb: &mut Framebuffer, selected: usize, editing: u8, view: &View<'_>) {
    const TRACK: u32 = 0xFF444444;
    const CHANNELS: [(&str, u32, u32); 3] = [
        ("R", 16, 0xFFE03030),
        ("G", 8, 0xFF30C030),
        ("B", 0, 0xFF3060E0),
    ];

    let style = view.style;
    let w = fb.width as i32;
    let color_for = |row: usize| if selected == row { WHITE } else { GRAY };

    // Header
    fill_rect(fb, 0, 0, w, 28, style.accent);
    draw_text(fb, 8, title_y(style), "Settings", 2, text_on(style.accent));

    // --- Volume (row 0) ---
    // Music and system volume, opened as their own screen.
    let y = 30;
    row_background(fb, y, 42, selected == 0, style.accent);
    draw_text(fb, 12, y + 4, "Volume", 2, color_for(0));

    let system = view.system_volume.map_or("--".to_string(), |v| format!("{v}%"));
    let pct = format!("M {}%  S {system}", style.volume_pct);
    draw_text(fb, 228 - text_width(&pct, 1), y + 8, &pct, 1, color_for(0));

    fill_rect(fb, 12, y + 23, 216, 6, TRACK);
    fill_rect(fb, 12, y + 23, 216 * style.volume_pct as i32 / 100, 6, style.accent);
    fill_rect(fb, 12, y + 32, 216, 6, TRACK);
    if let Some(v) = view.system_volume {
        fill_rect(fb, 12, y + 32, 216 * v as i32 / 100, 6, style.accent);
    }

    // --- Accent (row 1) ---
    let y = 72;
    row_background(fb, y, 76, selected == 1, style.accent);
    draw_text(fb, 12, y + 3, "Accent", 2, color_for(1));

    // Colour swatch with a border so dark colours are still visible.
    fill_rect(fb, 166, y + 2, 64, 18, GRAY);
    fill_rect(fb, 168, y + 4, 60, 14, style.accent);

    for (i, &(name, shift, bar_color)) in CHANNELS.iter().enumerate() {
        let value = ((style.accent >> shift) & 0xFF) as i32;
        let row_y = y + 21 + i as i32 * 18;
        let active = selected == 1 && editing as usize == i + 1;
        let text_color = if active { WHITE } else { GRAY };

        if active {
            draw_text(fb, 8, row_y, ">", 2, WHITE);
        }
        draw_text(fb, 26, row_y, name, 2, text_color);

        fill_rect(fb, 48, row_y + 2, 124, 12, TRACK);
        fill_rect(fb, 48, row_y + 2, 124 * value / 255, 12, bar_color);

        let text = value.to_string();
        draw_text(fb, 228 - text_width(&text, 2), row_y, &text, 2, text_color);
    }

    // --- Simple one-line rows (17 px each) ---
    let editing_row = |row: usize| selected == row && editing > 0;
    let value_text = |row: usize, text: String| {
        if editing_row(row) {
            format!("<{text}>")
        } else {
            text
        }
    };
    let rows: [(usize, &str, String); 7] = [
        (2, "Audio", view.output.label().to_string()),
        (3, "Bright", value_text(3, format!("{}%", style.brightness_pct))),
        (4, "Sleep", value_text(4, format_timeout(style.timeout_secs))),
        (5, "Battery", view.battery_driver.to_string()),
        (
            6,
            "Network",
            if !view.net_enabled {
                "off".to_string()
            } else {
                view.net_connected
                    .map(|name| truncate(name, 8))
                    .unwrap_or_else(|| "none".to_string())
            },
        ),
        (
            7,
            "Bluetooth",
            view.bt_connected
                .map(|name| truncate(name, 8))
                .unwrap_or_else(|| "none".to_string()),
        ),
        (8, "System", view.update_value.to_string()),
    ];

    for (i, (row, label, value)) in rows.iter().enumerate() {
        let y = 149 + i as i32 * 17;
        row_background(fb, y, 17, selected == *row, style.accent);
        draw_text(fb, 12, y + 1, label, 2, color_for(*row));

        // Battery driver, network and device names and update status can be
        // long, so they use the small font.
        let small = *row >= 5;
        let scale = if small { 1 } else { 2 };
        let text_y = if small { y + 5 } else { y + 1 };
        draw_text(fb, 228 - text_width(value, scale), text_y, value, scale, color_for(*row));
    }

    // Hint
    let hint = if editing > 0 {
        if selected == 1 {
            "1:next 2:- 3:+ 4:back"
        } else {
            "1:done 2:- 3:+ 4:back"
        }
    } else {
        match selected {
            1 | 3 | 4 => "1:edit 2:down 3:up 4:back",
            2 | 5 => "1:switch 2:down 3:up 4:back",
            0 | 6..=8 => "1:open 2:down 3:up 4:back",
            _ => "1:open 2:down 3:up 4:back",
        }
    };
    draw_centered_text(fb, 270, hint, 1, GRAY);
}

/// Volume screen: music (bman's own level) and system (the output device).
fn draw_volume(fb: &mut Framebuffer, selected: usize, editing: bool, view: &View<'_>) {
    const TRACK: u32 = 0xFF444444;
    let style = view.style;
    let w = fb.width as i32;

    fill_rect(fb, 0, 0, w, 28, style.accent);
    draw_text(fb, 8, title_y(style), "Volume", 2, text_on(style.accent));

    let rows: [(&str, Option<u8>, &str); 2] = [
        ("Music", Some(style.volume_pct), "bman's own level"),
        ("System", view.system_volume, "output device, e.g. earbuds"),
    ];
    for (i, (label, value, note)) in rows.iter().enumerate() {
        let y = 40 + i as i32 * 74;
        let active = selected == i;
        let color = if active { WHITE } else { GRAY };
        row_background(fb, y, 66, active, style.accent);
        draw_text(fb, 12, y + 6, label, 2, color);

        let text = match value {
            Some(v) if active && editing => format!("<{v}%>"),
            Some(v) => format!("{v}%"),
            None => "n/a".to_string(),
        };
        draw_text(fb, 228 - text_width(&text, 2), y + 6, &text, 2, color);

        fill_rect(fb, 12, y + 28, 216, 10, TRACK);
        if let Some(v) = value {
            fill_rect(fb, 12, y + 28, 216 * *v as i32 / 100, 10, style.accent);
        }
        let note = if value.is_none() { "no system mixer found" } else { note };
        draw_text(fb, 12, y + 46, note, 1, GRAY);
    }

    let hint = if editing { "2:- 3:+ 1:done 4:back" } else { "1:edit 2:down 3:up 4:back" };
    draw_centered_text(fb, 270, hint, 1, GRAY);
}

/// On-screen keyboard: the text typed so far, and a character picker.
fn draw_text_input(fb: &mut Framebuffer, selected: usize, layout: usize, view: &View<'_>) {
    let style = view.style;
    let w = fb.width as i32;
    let header_text = text_on(style.accent);

    fill_rect(fb, 0, 0, w, 42, style.accent);
    draw_scrolling(fb, 8, title_y(style), view.input_title, 2, header_text, w - 16 - header_reserved(style), false, style.time_ms);
    draw_scrolling(fb, 8, 30, view.input_subtitle, 1, header_text, w - 16, false, style.time_ms);

    // Typed text, the end of it if it's long, with a cursor.
    const SHOWN: usize = 13;
    let chars: Vec<char> = view.input_text.chars().collect();
    let start = chars.len().saturating_sub(SHOWN);
    // Masked (sudo password): stars, except the last character typed.
    let shown: String = chars[start..]
        .iter()
        .enumerate()
        .map(|(i, &c)| if view.input_masked && start + i + 1 < chars.len() { '*' } else { c })
        .collect();
    fill_rect(fb, 8, 56, w - 16, 28, DARK_GRAY);
    draw_text(fb, 14, 62, &format!("{shown}_"), 2, WHITE);
    let count = format!("{} characters", chars.len());
    draw_text(fb, 8, 90, &count, 1, GRAY);
    // Current layout, top right of the text box.
    let layout_name = KEY_LAYOUTS[layout % KEY_LAYOUTS.len()].0;
    draw_text(fb, w - 8 - text_width(layout_name, 1), 90, layout_name, 1, style.accent);

    // Picker: the highlighted entry big in the middle, neighbours either side.
    // The mode key is labelled with the layout it switches to.
    let total = key_count(layout);
    let next_layout = KEY_LAYOUTS[(layout + 1) % KEY_LAYOUTS.len()].0;
    let label = |i: usize| match key_at(layout, i) {
        Key::Char(c) => c.to_string(),
        Key::Space => "spc".to_string(),
        Key::Enter => "OK".to_string(),
        Key::Mode => next_layout.to_string(),
    };

    let y = 130;
    let box_w = 64;
    let box_x = (w - box_w) / 2;
    fill_rect(fb, box_x, y - 8, box_w, 40, style.accent);
    let current = label(selected);
    let scale = if current.chars().count() > 1 { 2 } else { 3 };
    let text_h = 8 * scale as i32;
    draw_text(
        fb,
        (w - text_width(&current, scale)) / 2,
        y + 12 - text_h / 2,
        &current,
        scale,
        text_on(style.accent),
    );

    for offset in 1..=2i32 {
        for dir in [-1i32, 1] {
            let i = (selected as i32 + dir * offset).rem_euclid(total as i32) as usize;
            let text = label(i);
            let cx = w / 2 + dir * (box_w / 2 + 6 + (offset - 1) * 34 + 14);
            let color = if offset == 1 { WHITE } else { GRAY };
            draw_text(fb, cx - text_width(&text, 2) / 2, y + 4, &text, 2, color);
        }
    }

    draw_centered_text(fb, 196, "1:type  2:next  3:prev", 1, GRAY);
    draw_centered_text(fb, 210, "5:delete  4:cancel", 1, GRAY);
    draw_centered_text(fb, 230, &format!("OK: {}", view.input_ok), 1, GRAY);
    draw_centered_text(fb, 244, "3 from start: keys, OK, spc", 1, GRAY);
}

/// Controls that apply to the screen being shown.
fn help_lines(view: &View<'_>) -> Vec<String> {
    let target = target_name(view);

    let lines: Vec<String> = match view.gui {
        GUI::Menu { .. } => vec!["1: open the highlighted item".into()],
        GUI::Singles { .. } => vec![
            "1: play the list from this song".into(),
            format!("5: add song to {target}"),
        ],
        GUI::Albums { .. } => vec!["1: open the album".into()],
        GUI::Artists { .. } => vec!["1: open the artist's albums and singles".into()],
        GUI::Artist { .. } => vec![
            "1: open an album, or play a single".into(),
            format!("5: add a single to {target}"),
        ],
        GUI::Album { .. } => vec![
            "1: play the album from this song".into(),
            format!("5: add song to {target}"),
        ],
        GUI::Playlists { .. } => vec![
            "1: open a playlist, or create a new one".into(),
            "5: choose which playlist songs get added to (marked *)".into(),
        ],
        GUI::Playlist { .. } => vec![
            "1 on Rename playlist: type a new name".into(),
            "1: play the list from this song".into(),
            "5: remove the song from the playlist".into(),
        ],
        GUI::Queue { .. } => vec![
            "1 on Save as playlist: make a new playlist from the whole queue".into(),
            "1: jump to this song".into(),
            "5: remove the song from the queue".into(),
        ],
        GUI::NowPlaying => vec![
            "1: pause / resume".into(),
            "2: next song".into(),
            "3: previous song (restarts the song if past 3 s)".into(),
            "5: loop mode (off, track, all, random)".into(),
        ],
        GUI::Settings { .. } => vec![
            "1: change the highlighted setting (Volume opens music and system volume)".into(),
            "While editing: 2 less, 3 more, 1 next field or done".into(),
            "Audio and Battery: 1 switches to the next option".into(),
            "System: updates and start on boot".into(),
        ],
        GUI::Network { .. } => vec![
            "1 on Wi-Fi: turn it on or off".into(),
            "1 on a network: connect (asks for a password if needed), or disconnect".into(),
            "5 on a saved network (+): forget it".into(),
        ],
        GUI::TextInput { .. } => vec![
            "2 / 3: next / previous key".into(),
            "1: type it".into(),
            format!("Every layout ends with spc (space), OK ({}) and the switch key (abc, ABC, 123, #+=). 3 from the first letter reaches them", view.input_ok),
            "5: delete the last character".into(),
            "4: cancel".into(),
        ],
        GUI::Volume { .. } => vec![
            "Music: bman's own volume".into(),
            "System: the output device's volume (e.g. Bluetooth earbuds); saved and set again when they connect".into(),
            "1: edit, then 2 less, 3 more, 1 done".into(),
        ],
        GUI::System { .. } => vec![
            "1 on the first row: check for an update, or install it (restarts bman)".into(),
            "1 on Auto check: look for updates every minute".into(),
            "1 on Start on boot: add/remove a boot service (asks for the sudo password if needed)".into(),
            "Restart device / Shut down: press 1 twice".into(),
            "Restart bman: restart just the player".into(),
        ],
        GUI::UpdatePrompt { .. } => vec![
            "1 on Update now: download, install and restart".into(),
            "Not now or 4: ask again next time bman starts".into(),
        ],
        GUI::Bluetooth { .. } => vec![
            "1: scan, or pair and connect the highlighted device".into(),
            "1 on a connected device disconnects it".into(),
        ],
        GUI::Streaming { .. } => vec!["1: open the service".into()],
        GUI::YouTube { .. } => vec![
            "1 on Search: type what to look for".into(),
            "1 on Liked songs: play your likes. 5 on it: 5 on a song likes it instead of queueing (*)".into(),
            "1 on YouTube likes: your account's likes (needs youtube-cookies.txt)".into(),
            "1 on Set up: download yt-dlp and Deno (about 100 MB) and test them".into(),
            "Needs internet. Songs download while they play and are deleted after".into(),
        ],
        GUI::StreamResults { list: StreamList::Liked, .. } => vec![
            "1: play your liked songs from this one".into(),
            "5: unlike the song".into(),
        ],
        GUI::StreamResults { .. } => vec![
            "1: play the list from this song".into(),
            format!("5: add the song to {}", if view.yt_to_liked { "Liked songs" } else { "the queue" }),
            "Artist and album show once a song has loaded".into(),
        ],
    };

    lines
        .iter()
        .flat_map(|line| wrap_text(line, 28))
        .collect()
}

/// Split text into lines of at most `max_chars`, breaking at spaces.
fn wrap_text(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();

    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > max_chars {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }

    lines.iter().map(|l| truncate(l, max_chars)).collect()
}

/// Help overlay shown on a long press: the controls, then what they do here.
fn draw_help(fb: &mut Framebuffer, view: &View<'_>) {
    const GENERAL: [&str; 6] = [
        "1 press  : select/play/pause",
        "2 presses: move down",
        "3 presses: move up",
        "4 presses: back",
        "5 presses: action",
        "Hold     : this help",
    ];

    let style = view.style;
    let w = fb.width as i32;

    fill_rect(fb, 0, 0, w, 28, style.accent);
    draw_text(fb, 8, title_y(style), "Help", 2, text_on(style.accent));

    for (i, line) in GENERAL.iter().enumerate() {
        draw_text(fb, 8, 36 + i as i32 * 12, line, 1, GRAY);
    }

    fill_rect(fb, 8, 112, w - 16, 1, GRAY);
    draw_text(fb, 8, 118, "On this screen:", 1, WHITE);

    let mut y = 132;
    for line in help_lines(view) {
        if y > 244 {
            break;
        }
        draw_text(fb, 8, y, &line, 1, WHITE);
        y += 12;
    }

    draw_centered_text(fb, 262, "Press any button to close", 1, GRAY);
}

/// Short message at the bottom of the screen ("Added to queue").
fn draw_toast(fb: &mut Framebuffer, text: &str, style: Style) {
    let w = fb.width as i32;
    let h = fb.height as i32;

    let text = truncate(text, 26);
    let box_w = text_width(&text, 1) + 16;
    let x = (w - box_w) / 2;
    let y = h - 30;

    fill_rect(fb, x, y, box_w, 18, style.accent);
    draw_text(fb, x + 8, y + 5, &text, 1, text_on(style.accent));
}

// ---------------------------------------------------------------------------
// Battery indicator
// ---------------------------------------------------------------------------

/// Infinity sign, 11x5, for "no battery, always powered".
const INFINITY: [&str; 5] = [
    ".###...###.",
    "#...#.#...#",
    "#....#....#",
    "#...#.#...#",
    ".###...###.",
];

/// Lightning bolt, 7x8, drawn over the battery while charging.
const BOLT: [&str; 8] = [
    "....##.",
    "...##..",
    "..##...",
    ".######",
    "######.",
    "...##..",
    "..##...",
    ".##....",
];

/// Width on the right of a header bar the battery indicator uses (icon,
/// plus the percentage when there is one), with a small gap.
fn header_reserved(style: Style) -> i32 {
    const ICON: i32 = 6 + 22 + 2; // right margin, body, nub
    match style.battery {
        Battery::Level { percent, .. } => ICON + 4 + text_width(&format!("{percent}%"), 1) + 4,
        Battery::Unknown => ICON + 4 + text_width("?", 1) + 4,
        Battery::Unlimited => ICON + 4,
    }
}

/// Battery icon (and percentage) in the top right, on the accent header bar
/// every screen has.
fn draw_battery(fb: &mut Framebuffer, style: Style) {
    const BODY_W: i32 = 22;
    const BODY_H: i32 = 12;
    const INNER_W: i32 = BODY_W - 4;
    const INNER_H: i32 = BODY_H - 4;
    const LOW: u32 = 0xFFE53935;
    const CHARGING: u32 = 0xFF3DDC84;

    let w = fb.width as i32;
    let (ink, bg) = (text_on(style.accent), style.accent);

    let x = w - 6 - BODY_W - 2; // leave room for the 2px nub
    let y = 8;

    // Outline and nub
    fill_rect(fb, x, y, BODY_W, BODY_H, ink);
    fill_rect(fb, x + 1, y + 1, BODY_W - 2, BODY_H - 2, bg);
    fill_rect(fb, x + BODY_W, y + 3, 2, BODY_H - 6, ink);

    let (fill, fill_color, label) = match style.battery {
        Battery::Unlimited => (INNER_W, ink, None),
        Battery::Unknown => (0, ink, Some("?".to_string())),
        Battery::Level { percent, charging } => {
            let pct = percent.min(100) as i32;
            let mut fill = (INNER_W * pct + 99) / 100;
            if percent > 0 {
                fill = fill.max(1);
            }
            let color = if charging {
                CHARGING
            } else if percent <= 15 {
                LOW
            } else {
                ink
            };
            (fill, color, Some(format!("{percent}%")))
        }
    };

    fill_rect(fb, x + 2, y + 2, fill, INNER_H, fill_color);

    if style.battery == Battery::Unlimited {
        let gx = x + 2 + (INNER_W - 11) / 2;
        let gy = y + 2 + (INNER_H - 5) / 2;
        for (row, line) in INFINITY.iter().enumerate() {
            for (col, c) in line.chars().enumerate() {
                if c == '#' {
                    put_pixel(fb, gx + col as i32, gy + row as i32, bg);
                }
            }
        }
    }

    // Bolt while charging (including plugged in at 100%). Each pixel
    // contrasts with what's under it: black/white on the green fill, the
    // icon colour on the empty part, so it's always visible.
    if matches!(style.battery, Battery::Level { charging: true, .. }) {
        let gx = x + 2 + (INNER_W - 7) / 2;
        let gy = y + 2 + (INNER_H - 8) / 2;
        for (row, line) in BOLT.iter().enumerate() {
            for (col, c) in line.chars().enumerate() {
                if c == '#' {
                    let px = gx + col as i32;
                    let color = if px < x + 2 + fill { text_on(fill_color) } else { ink };
                    put_pixel(fb, px, gy + row as i32, color);
                }
            }
        }
    }

    // Percentage, right-aligned just left of the icon.
    if let Some(text) = label {
        let tx = x - 4 - text_width(&text, 1);
        draw_text(fb, tx, y + 2, &text, 1, ink);
    }
}

// ---------------------------------------------------------------------------
// Drawing helpers
// ---------------------------------------------------------------------------

/// Black or white text, whichever is readable on `color`.
fn text_on(color: u32) -> u32 {
    let r = ((color >> 16) & 0xFF) as f32;
    let g = ((color >> 8) & 0xFF) as f32;
    let b = (color & 0xFF) as f32;
    let luma = 0.299 * r + 0.587 * g + 0.114 * b;

    if luma > 150.0 {
        BLACK
    } else {
        WHITE
    }
}

fn put_pixel(framebuffer: &mut Framebuffer, x: i32, y: i32, color: u32) {
    if x >= 0 && y >= 0 && (x as u32) < framebuffer.width && (y as u32) < framebuffer.height {
        let index = (y as u32 * framebuffer.width + x as u32) as usize;
        framebuffer.pixels[index] = color;
    }
}

pub fn fill_rect(framebuffer: &mut Framebuffer, x: i32, y: i32, w: i32, h: i32, color: u32) {
    for py in y..y + h {
        for px in x..x + w {
            put_pixel(framebuffer, px, py, color);
        }
    }
}

fn draw_art(fb: &mut Framebuffer, art: &Art, x: i32, y: i32) {
    for ay in 0..art.size {
        for ax in 0..art.size {
            if let Some(&pixel) = art.pixels.get((ay * art.size + ax) as usize) {
                put_pixel(fb, x + ax as i32, y + ay as i32, pixel);
            }
        }
    }
}

fn text_width(text: &str, scale: i32) -> i32 {
    text.chars().count() as i32 * 8 * scale
}

/// Draw text with the built-in 8x8 font, scaled by an integer factor.
pub fn draw_text(framebuffer: &mut Framebuffer, x: i32, y: i32, text: &str, scale: i32, color: u32) {
    draw_text_clipped(framebuffer, x, y, text, scale, color, i32::MIN, i32::MAX);
}

/// Like draw_text, but only pixels with clip_left <= x < clip_right are drawn.
#[allow(clippy::too_many_arguments)]
fn draw_text_clipped(
    framebuffer: &mut Framebuffer,
    x: i32,
    y: i32,
    text: &str,
    scale: i32,
    color: u32,
    clip_left: i32,
    clip_right: i32,
) {
    let mut cursor_x = x;

    for c in text.chars() {
        // Skip glyphs that are completely outside the clip box.
        if cursor_x + 8 * scale > clip_left && cursor_x < clip_right {
            let glyph = BASIC_LEGACY[if c.is_ascii() { c as usize } else { '?' as usize }];

            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..8 {
                    if (bits >> col) & 1 == 1 {
                        let px = cursor_x + col * scale;
                        let x0 = px.max(clip_left);
                        let x1 = (px + scale).min(clip_right);
                        if x1 > x0 {
                            fill_rect(
                                framebuffer,
                                x0,
                                y + row as i32 * scale,
                                x1 - x0,
                                scale,
                                color,
                            );
                        }
                    }
                }
            }
        }

        cursor_x += 8 * scale;
    }
}

/// Draw text inside a box starting at `x` and `max_w` pixels wide. If it
/// fits it is drawn normally (centered if `center`). If not, it scrolls
/// sideways: pause, slide to the end, pause, jump back and repeat.
#[allow(clippy::too_many_arguments)]
fn draw_scrolling(
    fb: &mut Framebuffer,
    x: i32,
    y: i32,
    text: &str,
    scale: i32,
    color: u32,
    max_w: i32,
    center: bool,
    time_ms: u64,
) {
    let width = text_width(text, scale);

    if width <= max_w {
        let start = if center { x + (max_w - width) / 2 } else { x };
        draw_text(fb, start, y, text, scale, color);
        return;
    }

    // Seamless loop: the text is followed by a gap and a second copy, so
    // after one full cycle the copy sits exactly where the text started.
    // Pause at the start of each cycle so the beginning is readable.
    let gap = 32 * scale;
    let cycle = (width + gap) as u64;
    let slide_ms = cycle * 1000 / SCROLL_SPEED;
    let t = time_ms % (SCROLL_PAUSE_MS + slide_ms);

    let offset = if t < SCROLL_PAUSE_MS {
        0
    } else {
        ((t - SCROLL_PAUSE_MS) * SCROLL_SPEED / 1000) as i32
    };

    draw_text_clipped(fb, x - offset, y, text, scale, color, x, x + max_w);
    draw_text_clipped(fb, x - offset + cycle as i32, y, text, scale, color, x, x + max_w);
}

fn draw_centered_text(framebuffer: &mut Framebuffer, y: i32, text: &str, scale: i32, color: u32) {
    let x = (framebuffer.width as i32 - text_width(text, scale)) / 2;
    draw_text(framebuffer, x, y, text, scale, color);
}

/// Shorten to `max` chars, ending in ".." if it was cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max.saturating_sub(2)).collect();
        format!("{cut}..")
    }
}

/// "45s", "2m", "1m 30s"
fn format_timeout(secs: u32) -> String {
    if secs == 0 {
        return "Never".into();
    }
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m {s}s"),
    }
}

fn format_time(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

pub fn draw_circle(
    framebuffer: &mut Framebuffer,
    center_x: i32,
    center_y: i32,
    radius: i32,
    color: u32,
) {
    let radius_squared = radius * radius;

    for y in -radius..=radius {
        for x in -radius..=radius {
            if x * x + y * y <= radius_squared {
                put_pixel(framebuffer, center_x + x, center_y + y, color);
            }
        }
    }
}
