//! Battery level, with swappable drivers just like the display.
//!
//! Pick one with `battery=` in config.txt, or from the Battery row in
//! settings (it cycles through BATTERY_DRIVERS):
//!   auto     - Default. Uses sysfs if the kernel reports a battery, else
//!              pisugar if the server answers, else shows "no battery"
//!              (infinity). Re-checked on every poll, so hot-plugged
//!              batteries or a late-starting pisugar-server get picked up.
//!   none     - Always show a full battery with an infinity symbol.
//!   sysfs    - The standard Linux battery interface (/sys/class/power_supply).
//!              Laptops, and any HAT / fuel gauge with a kernel driver.
//!   pisugar  - PiSugar 2 / 3 (and other models supported by the PiSugar
//!              power manager). Needs `pisugar-server` running.
//!
//! To support new hardware, implement `BatterySource` and add one line to
//! BATTERY_DRIVERS. Config parsing and the settings screen pick it up.

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Every battery driver: (name used in config.txt / settings, constructor).
/// The first entry is the default.
pub const BATTERY_DRIVERS: &[(&str, fn() -> Box<dyn BatterySource>)] = &[
    ("auto", || Box::new(Auto)),
    ("none", || Box::new(NoBattery)),
    ("sysfs", || Box::new(Sysfs)),
    ("pisugar", || Box::new(PiSugar)),
];

pub const DEFAULT_BATTERY_DRIVER: &str = BATTERY_DRIVERS[0].0;

/// How often the level is re-read.
const POLL_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Battery {
    /// No battery to report: drawn as a full battery with an infinity sign.
    Unlimited,
    /// A real reading. `charging` means on external power: charging, or
    /// plugged in and full.
    Level { percent: u8, charging: bool },
    /// The driver couldn't read the battery (yet).
    Unknown,
}

pub trait BatterySource: Send {
    fn read(&mut self) -> Battery;
}

pub fn battery_driver_names() -> impl Iterator<Item = &'static str> {
    BATTERY_DRIVERS.iter().map(|(name, _)| *name)
}

pub fn create_battery(driver: &str) -> Result<Box<dyn BatterySource>, String> {
    let driver = driver.trim().to_lowercase();
    BATTERY_DRIVERS
        .iter()
        .find(|(name, _)| *name == driver)
        .map(|(_, make)| make())
        .ok_or_else(|| {
            format!(
                "Unknown battery driver '{driver}'. Available drivers: {}",
                battery_driver_names().collect::<Vec<_>>().join(", ")
            )
        })
}

/// The driver after `current` in BATTERY_DRIVERS, wrapping around.
pub fn next_battery_driver(current: &str) -> &'static str {
    let i = BATTERY_DRIVERS
        .iter()
        .position(|(name, _)| *name == current)
        .map_or(0, |i| (i + 1) % BATTERY_DRIVERS.len());
    BATTERY_DRIVERS[i].0
}

// ---------------------------------------------------------------------------
// Monitor: reads the driver on a background thread so a slow or missing
// service can never stall the UI. The driver can be swapped live.
// ---------------------------------------------------------------------------

pub struct BatteryMonitor {
    shared: Arc<Mutex<Battery>>,
    swap: Sender<Box<dyn BatterySource>>,
}

impl BatteryMonitor {
    pub fn new(mut source: Box<dyn BatterySource>) -> Self {
        // First reading up front so the first frame already has a value.
        let shared = Arc::new(Mutex::new(source.read()));
        let (swap, swap_rx) = mpsc::channel::<Box<dyn BatterySource>>();

        let writer = shared.clone();
        thread::spawn(move || loop {
            match swap_rx.recv_timeout(POLL_INTERVAL) {
                Ok(new_source) => source = new_source,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let reading = source.read();
            if let Ok(mut slot) = writer.lock() {
                *slot = reading;
            }
        });

        Self { shared, swap }
    }

    /// Switch to another driver. It is read right away on the monitor thread.
    pub fn set_source(&self, source: Box<dyn BatterySource>) {
        if let Ok(mut slot) = self.shared.lock() {
            *slot = Battery::Unknown;
        }
        let _ = self.swap.send(source);
    }

    pub fn get(&self) -> Battery {
        self.shared.lock().map(|b| *b).unwrap_or(Battery::Unknown)
    }
}

// ---------------------------------------------------------------------------
// Drivers
// ---------------------------------------------------------------------------

/// Uses whichever battery can be seen right now: sysfs, then pisugar, else
/// "no battery" (infinity). Checked on every read.
struct Auto;

impl BatterySource for Auto {
    fn read(&mut self) -> Battery {
        if !Sysfs::batteries().is_empty() {
            return Sysfs.read();
        }
        match PiSugar.read() {
            Battery::Unknown => Battery::Unlimited,
            level => level,
        }
    }
}

struct NoBattery;

impl BatterySource for NoBattery {
    fn read(&mut self) -> Battery {
        Battery::Unlimited
    }
}

/// Talks to `pisugar-server` (the PiSugar power manager) over its socket.
/// Commands: "get battery" -> percent, "get battery_charging" and
/// "get battery_power_plugged" -> true/false.
struct PiSugar;

const PISUGAR_SOCKET: &str = "/tmp/pisugar.sock";
const PISUGAR_TCP: &str = "127.0.0.1:8423";
const PISUGAR_TIMEOUT: Duration = Duration::from_millis(500);

impl PiSugar {
    /// Send one command and return the value after the last ':' of the reply
    /// (replies look like "battery: 63.2", older ones are just "63.2").
    fn command(cmd: &str) -> io::Result<String> {
        let line = match Self::command_unix(cmd) {
            Ok(line) => line,
            Err(_) => Self::command_tcp(cmd)?,
        };
        Ok(line.rsplit(':').next().unwrap_or("").trim().to_string())
    }

    #[cfg(unix)]
    fn command_unix(cmd: &str) -> io::Result<String> {
        use std::os::unix::net::UnixStream;
        let mut stream = UnixStream::connect(PISUGAR_SOCKET)?;
        stream.set_read_timeout(Some(PISUGAR_TIMEOUT))?;
        stream.set_write_timeout(Some(PISUGAR_TIMEOUT))?;
        stream.write_all(format!("{cmd}\n").as_bytes())?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        Ok(line)
    }

    #[cfg(not(unix))]
    fn command_unix(_cmd: &str) -> io::Result<String> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no unix sockets"))
    }

    fn command_tcp(cmd: &str) -> io::Result<String> {
        let addr = PISUGAR_TCP.parse().map_err(|_| io::ErrorKind::InvalidInput)?;
        let mut stream = TcpStream::connect_timeout(&addr, PISUGAR_TIMEOUT)?;
        stream.set_read_timeout(Some(PISUGAR_TIMEOUT))?;
        stream.set_write_timeout(Some(PISUGAR_TIMEOUT))?;
        stream.write_all(format!("{cmd}\n").as_bytes())?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        Ok(line)
    }
}

impl BatterySource for PiSugar {
    fn read(&mut self) -> Battery {
        let Ok(level) = Self::command("get battery") else {
            return Battery::Unknown;
        };
        let Ok(percent) = level.parse::<f32>() else {
            return Battery::Unknown;
        };
        let is_true = |cmd| {
            Self::command(cmd)
                .map(|v| v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        };
        // battery_charging goes false once full, so also check the cable.
        let charging = is_true("get battery_charging") || is_true("get battery_power_plugged");

        Battery::Level {
            percent: percent.round().clamp(0.0, 100.0) as u8,
            charging,
        }
    }
}

/// Reads the kernel's power_supply class. Every device with `type` = Battery
/// counts, except peripherals (mice, headsets) which have `scope` = Device.
/// Several batteries are combined by energy/charge when available.
struct Sysfs;

const SYSFS_POWER: &str = "/sys/class/power_supply";

impl Sysfs {
    fn batteries() -> Vec<std::path::PathBuf> {
        let Ok(entries) = std::fs::read_dir(SYSFS_POWER) else {
            return Vec::new();
        };
        let mut found: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                read_str(p, "type").as_deref() == Some("Battery")
                    && read_str(p, "scope").as_deref() != Some("Device")
                    && read_str(p, "present").as_deref() != Some("0")
            })
            .collect();
        found.sort();
        found
    }

    /// A charger is connected: a Mains/USB supply reports online=1.
    fn charger_online() -> bool {
        let Ok(entries) = std::fs::read_dir(SYSFS_POWER) else {
            return false;
        };
        entries.flatten().map(|e| e.path()).any(|p| {
            read_str(&p, "type").as_deref() != Some("Battery")
                && read_str(&p, "scope").as_deref() != Some("Device")
                && read_str(&p, "online").as_deref() == Some("1")
        })
    }
}

fn read_str(dir: &std::path::Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .map(|s| s.trim().to_string())
}

fn read_num(dir: &std::path::Path, name: &str) -> Option<f64> {
    read_str(dir, name)?.parse().ok()
}

impl BatterySource for Sysfs {
    fn read(&mut self) -> Battery {
        let bats = Self::batteries();
        if bats.is_empty() {
            return Battery::Unknown;
        }

        let mut now = 0.0;
        let mut full = 0.0;
        let mut caps = Vec::new();
        // Batteries report "Full" or "Not charging" once topped up while still
        // plugged in, so the charger's own entry is checked too.
        let mut charging = Self::charger_online();

        for b in &bats {
            if read_str(b, "status").as_deref() == Some("Charging") {
                charging = true;
            }
            let pair = read_num(b, "energy_now")
                .zip(read_num(b, "energy_full"))
                .or_else(|| read_num(b, "charge_now").zip(read_num(b, "charge_full")));
            match pair {
                Some((n, f)) if f > 0.0 => {
                    now += n;
                    full += f;
                }
                _ => {
                    if let Some(c) = read_num(b, "capacity") {
                        caps.push(c);
                    }
                }
            }
        }

        let percent = if caps.is_empty() && full > 0.0 {
            now / full * 100.0
        } else if !caps.is_empty() && full == 0.0 {
            caps.iter().sum::<f64>() / caps.len() as f64
        } else if !caps.is_empty() {
            // Mixed: weight each source equally.
            let total = caps.iter().sum::<f64>() + now / full * 100.0;
            total / (caps.len() + 1) as f64
        } else {
            return Battery::Unknown;
        };

        Battery::Level {
            percent: percent.round().clamp(0.0, 100.0) as u8,
            charging,
        }
    }
}
