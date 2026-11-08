//! Terminal display driver (`driver=terminal`, or `bman --terminal`): draws
//! the 240x280 screen into the terminal with true-colour half blocks (each
//! character is one pixel wide and two tall), so bman can be used over SSH
//! with no screen and no GUI. Keys act as the button.
//!
//! The picture is shrunk by whole steps until it fits the terminal (make the
//! font smaller or the window bigger for a sharper picture: full size needs
//! 240x141 characters). Only cells that changed are redrawn, which keeps it
//! light over SSH. While it runs, log messages go to `bman.log` instead of
//! the terminal.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::Duration;

use crate::buttons::Button;
use crate::render::{Display, Framebuffer};

const HELP: &str = "1/Enter select  2/\u{2193} down  3/\u{2191} up  4/\u{2190} back  5/a action  ? help  q quit";
const LOG_FILE: &str = "bman.log";

/// Where the picture goes and how much it's shrunk.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Layout {
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
            open: true,
            idle: false,
        })
    }

    fn draw_status(&self, out: &mut String, layout: Layout) {
        let text = if self.idle { "Screen asleep: press any key" } else { HELP };
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

/// Average colour of the `scale` x `scale` block at block coordinates (bx, by).
fn sample(fb: &Framebuffer, bx: usize, by: usize, scale: usize) -> u32 {
    let (w, h) = (fb.width as usize, fb.height as usize);
    if scale == 1 {
        return fb.pixels[by * w + bx];
    }
    let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
    for y in by * scale..((by + 1) * scale).min(h) {
        for x in bx * scale..((bx + 1) * scale).min(w) {
            let p = fb.pixels[y * w + x];
            r += (p >> 16) & 0xFF;
            g += (p >> 8) & 0xFF;
            b += p & 0xFF;
            n += 1;
        }
    }
    let n = n.max(1);
    (r / n) << 16 | (g / n) << 8 | (b / n)
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

        // Smallest whole-step shrink that fits.
        let mut scale = 1;
        while scale < 16
            && (w.div_ceil(scale) > term_cols || h.div_ceil(scale).div_ceil(2) > picture_rows)
        {
            scale += 1;
        }
        let out_w = w.div_ceil(scale);
        let out_h = h.div_ceil(scale);
        let out_rows = out_h.div_ceil(2);
        let x0 = term_cols.saturating_sub(out_w) / 2 + 1;

        let layout = Layout { scale, term_cols, term_rows };
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
        for row in 0..out_rows {
            for col in 0..out_w {
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
