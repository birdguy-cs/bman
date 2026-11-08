//! Terminal display driver (`driver=terminal`, or `bman --terminal`): draws
//! the 240x280 screen into the terminal with true-colour half blocks (each
//! character is one pixel wide and two tall), so bman can be used over SSH
//! with no screen and no GUI. Keys act as the button.
//!
//! The picture is shrunk until it fits the terminal (full size needs 240x141
//! characters); `-` / `+` zoom out / in from there, `0` goes back to fit.
//! Zooming in past the window crops the picture. Only cells that changed are
//! redrawn, which keeps it light over SSH. While it runs, log messages go to `bman.log` instead of
//! the terminal.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use crate::buttons::Button;
use crate::render::{Display, Framebuffer};

const HELP: &str = "1/Enter select  2/\u{2193} down  3/\u{2191} up  4/\u{2190} back  5/a action  ? help  -/+ zoom  q quit";

/// Shrink factors to choose from, biggest picture first.
const SCALES: [f32; 12] = [1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 3.5, 4.0, 5.0, 6.0, 8.0];
const LOG_FILE: &str = "bman.log";

/// At most this many terminal updates a second. Over SSH every update is
/// Wi-Fi traffic, and on a Pi Zero 2 W Wi-Fi shares its radio with
/// Bluetooth, so constant redraws (scrolling text) can make Bluetooth audio
/// stutter. Keys are still read every frame.
const MIN_REDRAW: Duration = Duration::from_millis(100);

/// Where the picture goes and how much it's shrunk.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Layout {
    /// Index into SCALES.
    scale: usize,
    term_cols: usize,
    term_rows: usize,
}

pub struct TerminalDisplay {
    original: libc::termios,
    /// The real stderr, put back when the driver closes.
    saved_stderr: i32,
    /// Last drawn (top, bottom) colour of every cell; cleared to force a redraw.
    cells: Vec<(u32, u32)>,
    layout: Option<Layout>,
    /// Zoom chosen with -/+ (index into SCALES); None = fit the window.
    zoom: Option<usize>,
    last_redraw: Option<Instant>,
    open: bool,
    idle: bool,
}

impl TerminalDisplay {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let interactive = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
        if !interactive {
            return Err("The terminal driver needs an interactive terminal (run it in SSH, not as a service)".into());
        }

        // Raw mode: keys arrive one at a time, unechoed, and reads don't wait.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut original) } != 0 {
            return Err("Couldn't read terminal settings".into());
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) };

        // Log lines would scribble over the picture: send them to a file.
        let saved_stderr = unsafe { libc::dup(2) };
        if let Ok(file) = OpenOptions::new().create(true).append(true).open(LOG_FILE) {
            unsafe { libc::dup2(file.as_raw_fd(), 2) };
        }

        // Alternate screen (the shell's contents come back on exit), no cursor.
        write_out("\x1b[?1049h\x1b[?25l\x1b[2J");

        Ok(Self {
            original,
            saved_stderr,
            cells: Vec::new(),
            layout: None,
            zoom: None,
            last_redraw: None,
            open: true,
            idle: false,
        })
    }

    /// Zoom in effect: the one picked with -/+, else what fit last frame.
    /// (Several presses can arrive between frames, so they build on each other.)
    fn current_zoom(&self) -> usize {
        self.zoom.or(self.layout.map(|l| l.scale)).unwrap_or(0)
    }

    fn draw_status(&self, out: &mut String, layout: Layout) {
        let text = if self.idle {
            "Screen asleep: press any key".to_string()
        } else {
            let zoom = (100.0 / SCALES[layout.scale]).round();
            let mode = if self.zoom.is_some() { "0 fit" } else { "fit" };
            format!("{HELP}  [{zoom}% {mode}]")
        };
        let shown: String = text.chars().take(layout.term_cols).collect();
        let col = (layout.term_cols - shown.chars().count()) / 2 + 1;
        out.push_str(&format!(
            "\x1b[{};1H\x1b[0m\x1b[2K\x1b[{};{}H\x1b[38;5;245m{}\x1b[0m",
            layout.term_rows, layout.term_rows, col, shown
        ));
    }
}

impl Drop for TerminalDisplay {
    fn drop(&mut self) {
        write_out("\x1b[0m\x1b[?25h\x1b[?1049l");
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.original);
            if self.saved_stderr >= 0 {
                libc::dup2(self.saved_stderr, 2);
                libc::close(self.saved_stderr);
            }
        }
        eprintln!("(bman's log while in the terminal driver is in {LOG_FILE})");
    }
}

fn write_out(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

/// Terminal size in characters (columns, rows).
fn term_size() -> (usize, usize) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col as usize, ws.ws_row as usize)
    } else {
        (80, 24)
    }
}

/// Source pixel range covered by output pixel `i` at shrink factor `scale`.
fn span(i: usize, scale: f32, len: usize) -> (usize, usize) {
    let start = ((i as f32 * scale) as usize).min(len - 1);
    let end = (((i + 1) as f32 * scale) as usize).clamp(start + 1, len);
    (start, end)
}

/// Average colour of the source pixels under output pixel (ox, oy).
fn sample(fb: &Framebuffer, ox: usize, oy: usize, scale: f32) -> u32 {
    let (w, h) = (fb.width as usize, fb.height as usize);
    let (x0, x1) = span(ox, scale, w);
    let (y0, y1) = span(oy, scale, h);
    if x1 - x0 == 1 && y1 - y0 == 1 {
        return fb.pixels[y0 * w + x0];
    }
    let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
    for y in y0..y1 {
        for x in x0..x1 {
            let p = fb.pixels[y * w + x];
            r += (p >> 16) & 0xFF;
            g += (p >> 8) & 0xFF;
            b += p & 0xFF;
            n += 1;
        }
    }
    (r / n) << 16 | (g / n) << 8 | (b / n)
}

/// Picture size in (columns, pixel rows) at a shrink factor.
fn out_size(w: usize, h: usize, scale: f32) -> (usize, usize) {
    ((w as f32 / scale).ceil() as usize, (h as f32 / scale).ceil() as usize)
}

fn rgb(c: u32) -> (u32, u32, u32) {
    ((c >> 16) & 0xFF, (c >> 8) & 0xFF, c & 0xFF)
}

impl Display for TerminalDisplay {
    fn size(&self) -> (u32, u32) {
        (240, 280)
    }

    fn present(&mut self, fb: &Framebuffer) -> Result<(), Box<dyn std::error::Error>> {
        let (w, h) = (fb.width as usize, fb.height as usize);
        let (term_cols, term_rows) = term_size();
        let picture_rows = term_rows.saturating_sub(1).max(1); // last row: key help

        // The biggest picture that fits, unless zoomed with -/+.
        let fits = |i: usize| {
            let (ow, oh) = out_size(w, h, SCALES[i]);
            ow <= term_cols && oh.div_ceil(2) <= picture_rows
        };
        let fit = (0..SCALES.len()).find(|&i| fits(i)).unwrap_or(SCALES.len() - 1);
        let index = self.zoom.unwrap_or(fit);
        let scale = SCALES[index];
        let (out_w, out_h) = out_size(w, h, scale);
        let out_rows = out_h.div_ceil(2);
        let x0 = term_cols.saturating_sub(out_w) / 2 + 1;
        // Zoomed in past the window: draw only what fits (cropped).
        let shown_cols = out_w.min(term_cols);
        let shown_rows = out_rows.min(picture_rows);

        let layout = Layout { scale: index, term_cols, term_rows };
        // Throttle redraws (see MIN_REDRAW); a new size or zoom always draws.
        let recent = self.last_redraw.is_some_and(|t| t.elapsed() < MIN_REDRAW);
        if recent && self.layout == Some(layout) {
            return Ok(());
        }
        self.last_redraw = Some(Instant::now());
        let mut out = String::new();
        if self.layout != Some(layout) {
            // New size: clear everything and redraw every cell.
            self.layout = Some(layout);
            self.cells = vec![(u32::MAX, u32::MAX); out_w * out_rows];
            out.push_str("\x1b[0m\x1b[2J");
            self.draw_status(&mut out, layout);
        }

        let mut cursor: Option<(usize, usize)> = None;
        let (mut fg, mut bg) = (u32::MAX, u32::MAX);
        for row in 0..shown_rows {
            for col in 0..shown_cols {
                let top = sample(fb, col, row * 2, scale) & 0xFFFFFF;
                let bottom = if row * 2 + 1 < out_h {
                    sample(fb, col, row * 2 + 1, scale) & 0xFFFFFF
                } else {
                    0
                };
                let cell = &mut self.cells[row * out_w + col];
                if *cell == (top, bottom) {
                    continue;
                }
                *cell = (top, bottom);

                if cursor != Some((row, col)) {
                    out.push_str(&format!("\x1b[{};{}H", row + 1, x0 + col));
                }
                if fg != top {
                    let (r, g, b) = rgb(top);
                    out.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
                    fg = top;
                }
                if bg != bottom {
                    let (r, g, b) = rgb(bottom);
                    out.push_str(&format!("\x1b[48;2;{r};{g};{b}m"));
                    bg = bottom;
                }
                out.push('\u{2580}'); // upper half block: top = fg, bottom = bg
                cursor = Some((row, col + 1));
            }
        }

        if !out.is_empty() {
            out.push_str("\x1b[0m");
            write_out(&out);
        }
        Ok(())
    }

    fn poll_buttons(&mut self) -> Vec<Button> {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n as usize]);
        }

        let mut buttons = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            i += 1;
            let button = match b {
                0x03 | b'q' | b'Q' => {
                    self.open = false;
                    None
                }
                b'1' | b'\r' | b'\n' | b' ' | b'l' => Some(Button::Select),
                b'2' | b'j' | b's' => Some(Button::Down),
                b'3' | b'k' | b'w' => Some(Button::Up),
                b'4' | 0x7F | 0x08 | b'h' | b'b' => Some(Button::Back),
                b'5' | b'a' | b'x' => Some(Button::Action),
                b'?' => Some(Button::Help),
                // Zoom: - smaller picture, + bigger, 0 back to fit.
                b'-' | b'_' => {
                    self.zoom = Some((self.current_zoom() + 1).min(SCALES.len() - 1));
                    None
                }
                b'+' | b'=' => {
                    self.zoom = Some(self.current_zoom().saturating_sub(1));
                    None
                }
                b'0' => {
                    self.zoom = None;
                    None
                }
                // Arrow keys: ESC [ A..D (or ESC O A..D). A lone ESC is Back.
                0x1B => match (bytes.get(i), bytes.get(i + 1)) {
                    (Some(b'[' | b'O'), Some(&key)) => {
                        i += 2;
                        match key {
                            b'A' => Some(Button::Up),
                            b'B' => Some(Button::Down),
                            b'C' => Some(Button::Select),
                            b'D' => Some(Button::Back),
                            _ => None,
                        }
                    }
                    _ => Some(Button::Back),
                },
                _ => None,
            };
            buttons.extend(button);
        }
        buttons
    }

    fn is_open(&self) -> bool {
        self.open
    }

    /// On the Pi the real button keeps working alongside the keyboard.
    fn wants_gpio_buttons(&self) -> bool {
        true
    }

    fn set_idle(&mut self, idle: bool) {
        self.idle = idle;
        if let Some(layout) = self.layout {
            let mut out = String::new();
            self.draw_status(&mut out, layout);
            write_out(&out);
        }
    }

    fn frame_interval(&self) -> Duration {
        Duration::from_millis(50)
    }
}
