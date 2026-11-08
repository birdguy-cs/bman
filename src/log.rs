//! Coloured console logging: `info!`, `warn!`, `error!` (same arguments as
//! `println!`). Colour is used only when the output is a terminal and
//! NO_COLOR isn't set, so piping to a file stays plain.

use std::fmt::Arguments;
use std::io::{IsTerminal, Write};

#[derive(Clone, Copy)]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// Checked every time: the terminal display driver points stderr at a log
/// file while it runs.
fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}

pub fn log(level: Level, args: Arguments) {
    let (tag, color) = match level {
        Level::Info => ("info ", "\x1b[32m"),  // green
        Level::Warn => ("warn ", "\x1b[33m"),  // yellow
        Level::Error => ("error", "\x1b[31m"), // red
    };
    let mut err = std::io::stderr().lock();
    let _ = if color_enabled() {
        match level {
            // Only the tag is coloured for info; warnings and errors colour the whole line.
            Level::Info => writeln!(err, "{color}\x1b[1m{tag}\x1b[0m {args}"),
            _ => writeln!(err, "{color}\x1b[1m{tag}\x1b[22m {args}\x1b[0m"),
        }
    } else {
        writeln!(err, "{tag} {args}")
    };
}

macro_rules! info {
    ($($arg:tt)*) => { $crate::log::log($crate::log::Level::Info, format_args!($($arg)*)) };
}
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::log($crate::log::Level::Warn, format_args!($($arg)*)) };
}
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::log($crate::log::Level::Error, format_args!($($arg)*)) };
}
