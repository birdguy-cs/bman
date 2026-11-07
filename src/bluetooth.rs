use std::{
    process::Command,
    sync::{Arc, Mutex, MutexGuard},
    thread,
};

/// How long a scan listens for devices.
const SCAN_SECONDS: u64 = 8;

#[derive(Clone, Debug)]
pub struct BtDevice {
    pub mac: String,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
}

#[derive(Default)]
struct Shared {
    devices: Vec<BtDevice>,
    status: String,
    busy: bool,
}

/// Bluetooth pairing/connecting through BlueZ's `bluetoothctl`.
/// Every slow operation runs on a background thread, so the UI never blocks.
/// Audio then goes out through the system's default audio device (PipeWire
/// or PulseAudio switch to the headphones when they connect), which is what
/// the "Wireless" audio output uses.
#[derive(Clone)]
pub struct Bluetooth {
    shared: Arc<Mutex<Shared>>,
}

impl Bluetooth {
    pub fn new() -> Self {
        let bt = Self {
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        bt.refresh();
        bt
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_status(&self, status: impl Into<String>) {
        self.lock().status = status.into();
    }

    /// Paired and discovered devices: connected first, then paired, then by name.
    pub fn devices(&self) -> Vec<BtDevice> {
        self.lock().devices.clone()
    }

    pub fn status(&self) -> String {
        self.lock().status.clone()
    }

    pub fn is_busy(&self) -> bool {
        self.lock().busy
    }

    /// Re-read the device list in the background.
    pub fn refresh(&self) {
        self.run_job("Checking...", |bt| {
            bt.update_devices();
            if bt.status() == "Checking..." {
                bt.set_status("");
            }
        });
    }

    /// Search for nearby devices for a few seconds, then update the list.
    pub fn scan(&self) {
        self.run_job("Scanning...", |bt| {
            let _ = ctl(&[], &["power", "on"], 5);

            if let Err(e) = ctl(&[], &["scan", "on"], SCAN_SECONDS) {
                bt.set_status(e);
                return;
            }

            bt.update_devices();
            let count = bt.lock().devices.len();
            bt.set_status(format!("Found {count} devices"));
        });
    }

    /// Connected device -> disconnect. Paired -> connect.
    /// New device -> pair, trust, then connect.
    pub fn activate(&self, mac: String) {
        let device = self.lock().devices.iter().find(|d| d.mac == mac).cloned();
        let Some(device) = device else {
            return;
        };

        self.run_job("Working...", move |bt| {
            if device.connected {
                match ctl(&[], &["disconnect", &device.mac], 10) {
                    Ok(out) if out.contains("Successful disconnected") || out.contains("successful") => {
                        bt.set_status(format!("Disconnected {}", device.name))
                    }
                    Ok(_) => bt.set_status("Disconnect failed"),
                    Err(e) => bt.set_status(e),
                }
                bt.update_devices();
                return;
            }

            if !device.paired {
                bt.set_status(format!("Pairing {}...", device.name));

                // NoInputNoOutput handles headphones/speakers that pair
                // without a PIN.
                let result = ctl(&["--agent", "NoInputNoOutput"], &["pair", &device.mac], 30);
                match result {
                    Ok(out)
                        if out.contains("Pairing successful")
                            || out.contains("AlreadyExists")
                            || out.contains("Already Exists") => {}
                    Ok(_) => {
                        bt.set_status("Pairing failed");
                        bt.update_devices();
                        return;
                    }
                    Err(e) => {
                        bt.set_status(e);
                        return;
                    }
                }
            }

            // Trusted devices reconnect on their own after a reboot.
            let _ = ctl(&[], &["trust", &device.mac], 10);

            bt.set_status(format!("Connecting {}...", device.name));
            match ctl(&[], &["connect", &device.mac], 20) {
                Ok(out) if out.contains("Connection successful") => {
                    bt.set_status(format!("Connected {}", device.name))
                }
                Ok(_) => bt.set_status("Connect failed"),
                Err(e) => bt.set_status(e),
            }
            bt.update_devices();
        });
    }

    /// Run `job` on a thread unless another job is already running.
    fn run_job(&self, status: &str, job: impl FnOnce(&Bluetooth) + Send + 'static) {
        {
            let mut shared = self.lock();
            if shared.busy {
                return;
            }
            shared.busy = true;
            shared.status = status.to_string();
        }

        let bt = self.clone();
        thread::spawn(move || {
            job(&bt);
            bt.lock().busy = false;
        });
    }

    fn update_devices(&self) {
        let all = match ctl(&[], &["devices"], 5) {
            Ok(out) => out,
            Err(e) => {
                self.set_status(e);
                return;
            }
        };

        // `devices Paired` / `devices Connected` need BlueZ 5.65+
        // (Raspberry Pi OS Bookworm).
        let paired = parse_devices(&ctl(&[], &["devices", "Paired"], 5).unwrap_or_default());
        let connected = parse_devices(&ctl(&[], &["devices", "Connected"], 5).unwrap_or_default());

        let mut devices: Vec<BtDevice> = parse_devices(&all)
            .into_iter()
            .filter(|(mac, name)| !name.is_empty() && name.replace('-', ":") != *mac)
            .map(|(mac, name)| BtDevice {
                paired: paired.iter().any(|(m, _)| *m == mac),
                connected: connected.iter().any(|(m, _)| *m == mac),
                mac,
                name,
            })
            .collect();

        devices.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then(b.paired.cmp(&a.paired))
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        self.lock().devices = devices;
    }
}

/// Run `bluetoothctl [pre...] --timeout N args...` and return its output.
fn ctl(pre: &[&str], args: &[&str], timeout_secs: u64) -> Result<String, String> {
    let output = Command::new("bluetoothctl")
        .args(pre)
        .arg("--timeout")
        .arg(timeout_secs.to_string())
        .args(args)
        .output()
        .map_err(|e| format!("Bluetooth unavailable: {e}"))?;

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse lines like "Device AA:BB:CC:DD:EE:FF My Headphones".
fn parse_devices(output: &str) -> Vec<(String, String)> {
    let mut devices = Vec::new();

    for line in output.lines() {
        let Some(pos) = line.find("Device ") else {
            continue;
        };

        let rest = &line[pos + "Device ".len()..];
        let mut parts = rest.splitn(2, ' ');
        let mac = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("").trim();

        if mac.len() == 17 && mac.matches(':').count() == 5 {
            devices.push((mac.to_string(), name.to_string()));
        }
    }

    devices
}
