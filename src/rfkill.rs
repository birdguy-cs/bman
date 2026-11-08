//! rfkill: radios switched off in software (e.g. Raspberry Pi OS blocks
//! Bluetooth / Wi-Fi until set up, and then `bluetoothctl power on` fails
//! with "off-blocked"). Anyone can read the state from /sys/class/rfkill;
//! unblocking needs root, so it goes through the sudo helper (root.rs) as
//! `bman --rfkill bluetooth|wlan`, which writes 0 to each matching device's
//! `soft` file. A hard block is a physical switch and can't be undone here.

use std::fs;
use std::path::Path;

use crate::root::{RootError, euid, run_helper};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Radio {
    Bluetooth,
    Wifi,
}

impl Radio {
    /// rfkill's name for the type.
    fn kind(self) -> &'static str {
        match self {
            Radio::Bluetooth => "bluetooth",
            Radio::Wifi => "wlan",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Radio::Bluetooth => "Bluetooth",
            Radio::Wifi => "Wi-Fi",
        }
    }
}

#[derive(PartialEq, Eq)]
pub enum Block {
    None,
    Soft,
    /// Physical switch or firmware: software can't unblock it.
    Hard,
}

/// The rfkill state of every device of `kind` ("bluetooth", "wlan"):
/// (soft, hard) per device.
fn devices(kind: &str) -> Vec<(std::path::PathBuf, bool, bool)> {
    devices_in(Path::new(SYSFS), kind)
}

const SYSFS: &str = "/sys/class/rfkill";

fn devices_in(base: &Path, kind: &str) -> Vec<(std::path::PathBuf, bool, bool)> {
    let read = |p: std::path::PathBuf| fs::read_to_string(p).unwrap_or_default().trim().to_string();
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| read(p.join("type")) == kind)
        .map(|p| {
            let soft = read(p.join("soft")) == "1";
            let hard = read(p.join("hard")) == "1";
            (p, soft, hard)
        })
        .collect()
}

pub fn state(radio: Radio) -> Block {
    let devices = devices(radio.kind());
    if devices.iter().any(|(_, _, hard)| *hard) {
        Block::Hard
    } else if devices.iter().any(|(_, soft, _)| *soft) {
        Block::Soft
    } else {
        Block::None
    }
}

/// Unblock a radio, through sudo when not root. Blocking: call from a thread.
pub fn unblock(radio: Radio, password: Option<&str>) -> Result<String, RootError> {
    if euid() == 0 {
        return unblock_as_root(radio.kind()).map_err(RootError::Failed);
    }
    run_helper(&["--rfkill".into(), radio.kind().into()], password)
}

fn unblock_as_root(kind: &str) -> Result<String, String> {
    unblock_in(Path::new(SYSFS), kind)
}

fn unblock_in(base: &Path, kind: &str) -> Result<String, String> {
    let devices = devices_in(base, kind);
    if devices.is_empty() {
        return Err(format!("No {kind} radio found"));
    }
    for (path, _, _) in &devices {
        fs::write(path.join("soft"), "0").map_err(|e| format!("Can't unblock: {e}"))?;
    }
    let label = if kind == "wlan" { "Wi-Fi" } else { "Bluetooth" };
    info!("Unblocked {label} (rfkill)");
    Ok(format!("{label} unblocked"))
}

/// Root side: `bman --rfkill bluetooth|wlan`.
pub fn cli_root(args: &[String]) -> Result<String, String> {
    match args.get(2).map(String::as_str) {
        Some(kind @ ("bluetooth" | "wlan")) => unblock_as_root(kind),
        _ => Err("Usage: --rfkill bluetooth|wlan".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unblocks_only_the_soft_blocked_kind() {
        let base = std::env::temp_dir().join(format!("bman-rfkill-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for (name, kind, soft) in [("rfkill0", "bluetooth", "1"), ("rfkill1", "wlan", "1")] {
            let dir = base.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
            fs::write(dir.join("soft"), format!("{soft}\n")).unwrap();
            fs::write(dir.join("hard"), "0\n").unwrap();
        }
        assert!(devices_in(&base, "bluetooth")[0].1);
        assert_eq!(unblock_in(&base, "bluetooth").unwrap(), "Bluetooth unblocked");
        assert!(!devices_in(&base, "bluetooth")[0].1);
        assert!(devices_in(&base, "wlan")[0].1); // Wi-Fi left alone
        assert!(unblock_in(&base, "nfc").is_err());
        fs::remove_dir_all(&base).unwrap();
    }
}

#[cfg(test)]
mod live {
    /// Uses whatever `sudo` is first in PATH: `PATH=/tmp/fake:$PATH cargo test rfkill_helper -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn rfkill_helper() {
        match super::unblock(super::Radio::Bluetooth, None) {
            Ok(m) => println!("result: ok {m}"),
            Err(_) => println!("result: err"),
        }
    }
}
