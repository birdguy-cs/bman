//! Battery level, with swappable drivers just like the display.
//!
//! Pick one with `battery=` in config.txt:
//!   none     - no battery to read (mains power, a PC, or a UPS that can't
//!              report its level such as the PiSugar S). Shows a full battery
//!              with an infinity symbol. This is the default.
//!   pisugar  - PiSugar 2 / 3 (and other models supported by the PiSugar
//!              power manager). Needs `pisugar-server` running.
//!
//! To support new hardware, implement `BatterySource`, add its name to
//! BATTERY_DRIVERS and add a match arm in `create_battery`.

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Names accepted by `battery=` in config.txt.
pub const BATTERY_DRIVERS: &[&str] = &["none", "pisugar"];

/// How often the level is re-read.
const POLL_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Battery {
    /// No battery to report: drawn as a full battery with an infinity sign.
    Unlimited,
    /// A real reading.
    Level { percent: u8, charging: bool },
    /// The driver couldn't read the battery (yet).
    Unknown,
}

pub trait BatterySource: Send {
    fn read(&mut self) -> Battery;
}

pub fn create_battery(driver: &str) -> Result<Box<dyn BatterySource>, String> {
    match driver.trim().to_lowercase().as_str() {
        "none" => Ok(Box::new(NoBattery)),
        "pisugar" => Ok(Box::new(PiSugar)),
        other => Err(format!(
            "Unknown battery driver '{other}'. Available drivers: {}",
            BATTERY_DRIVERS.join(", ")
        )),
    }
}

// ---------------------------------------------------------------------------
// Monitor: reads the driver on a background thread so a slow or missing
// service can never stall the UI.
// ---------------------------------------------------------------------------

pub struct BatteryMonitor {
    shared: Arc<Mutex<Battery>>,
}

impl BatteryMonitor {
    pub fn new(mut source: Box<dyn BatterySource>) -> Self {
        // First reading up front so the first frame already has a value.
        let shared = Arc::new(Mutex::new(source.read()));

        let writer = shared.clone();
        thread::spawn(move || loop {
            thread::sleep(POLL_INTERVAL);
            let reading = source.read();
            if let Ok(mut slot) = writer.lock() {
                *slot = reading;
            }
        });

        Self { shared }
    }

    pub fn get(&self) -> Battery {
        self.shared.lock().map(|b| *b).unwrap_or(Battery::Unknown)
    }
}

// ---------------------------------------------------------------------------
// Drivers
// ---------------------------------------------------------------------------

struct NoBattery;

impl BatterySource for NoBattery {
    fn read(&mut self) -> Battery {
        Battery::Unlimited
    }
}

/// Talks to `pisugar-server` (the PiSugar power manager) over its socket.
/// Commands: "get battery" -> percent, "get battery_charging" -> true/false.
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
        let charging = Self::command("get battery_charging")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        Battery::Level {
            percent: percent.round().clamp(0.0, 100.0) as u8,
            charging,
        }
    }
}
