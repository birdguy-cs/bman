//! Bluetooth through BlueZ's `bluetoothctl`.
//!
//! Status queries (device list, paired/connected) use one-shot
//! `bluetoothctl --timeout N <cmd>` calls. Pairing and connecting run in one
//! interactive `bluetoothctl` session, so the pairing agent stays registered,
//! prompts ("Authorize service (yes/no)") can be answered, and a device that
//! dropped out of BlueZ's cache can be found again with a scan in the same
//! session.
//!
//! Every slow operation runs on a background thread so the UI never blocks.
//! Audio goes to the system's default output, which PipeWire / PulseAudio
//! switch to the headphones when they connect. The "Wireless" output uses it.

use std::{
    io::{Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

/// How long a scan listens for devices.
const SCAN_SECONDS: u64 = 8;
/// How often the connection state is re-checked in the background, so
/// headphones turning off (or reconnecting on their own) are noticed.
const POLL_SLOW: Duration = Duration::from_secs(10);
/// Re-check interval while a Bluetooth / settings screen is open.
const POLL_FAST: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub struct BtDevice {
    pub mac: String,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
}

/// Connection changes, picked up by the app with `take_events`.
#[derive(Clone, Debug)]
pub enum BtEvent {
    Connected(String),
    Disconnected(String),
}

#[derive(Default)]
struct Shared {
    devices: Vec<BtDevice>,
    status: String,
    busy: bool,
    events: Vec<BtEvent>,
    /// Set after the first device list is read, so connections that already
    /// existed at startup still produce a Connected event.
    last_refresh: Option<Instant>,
}

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

    /// Connection changes since the last call.
    pub fn take_events(&self) -> Vec<BtEvent> {
        std::mem::take(&mut self.lock().events)
    }

    /// Call every frame. Re-reads the device list in the background now and
    /// then (`fast` while a Bluetooth-related screen is open).
    pub fn poll(&self, fast: bool) {
        let due = {
            let shared = self.lock();
            let interval = if fast { POLL_FAST } else { POLL_SLOW };
            !shared.busy && shared.last_refresh.is_some_and(|t| t.elapsed() >= interval)
        };
        if due {
            self.run_job(None, |bt| bt.update_devices());
        }
    }

    /// Re-read the device list in the background.
    pub fn refresh(&self) {
        self.run_job(Some("Checking..."), |bt| {
            bt.update_devices();
            if bt.status() == "Checking..." {
                bt.set_status("");
            }
        });
    }

    /// Search for nearby devices for a few seconds, then update the list.
    pub fn scan(&self) {
        self.run_job(Some("Scanning..."), |bt| {
            if let Err(e) = power_on() {
                bt.set_status(e);
                return;
            }

            // Here bluetoothctl's --timeout is what we want: listen this long.
            let secs = SCAN_SECONDS.to_string();
            if let Err(e) = ctl(&["--timeout", &secs, "scan", "on"], SCAN_SECONDS + 5) {
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

        self.run_job(Some("Working..."), move |bt| {
            let result = match Session::open() {
                Ok(mut session) => {
                    if device.connected {
                        session.disconnect(bt, &device)
                    } else {
                        session.pair_and_connect(bt, &device)
                    }
                }
                Err(e) => Err(e),
            };
            if let Err(e) = result {
                bt.set_status(e);
            }
            bt.update_devices();
        });
    }

    /// Run `job` on a thread unless another job is already running.
    /// `status` (if any) is shown while it runs.
    fn run_job(&self, status: Option<&str>, job: impl FnOnce(&Bluetooth) + Send + 'static) {
        {
            let mut shared = self.lock();
            if shared.busy {
                return;
            }
            shared.busy = true;
            if let Some(status) = status {
                shared.status = status.to_string();
            }
        }

        let bt = self.clone();
        thread::spawn(move || {
            job(&bt);
            bt.lock().busy = false;
        });
    }

    fn update_devices(&self) {
        let all = match ctl(&["devices"], 5) {
            Ok(out) => out,
            Err(e) => {
                let mut shared = self.lock();
                shared.status = e;
                shared.last_refresh = Some(Instant::now());
                return;
            }
        };

        let paired = paired_macs();
        // `info` works on every BlueZ version, unlike `devices Connected`.
        let connected: Vec<String> = paired
            .iter()
            .filter(|mac| {
                ctl(&["info", mac], 5).is_ok_and(|out| out.contains("Connected: yes"))
            })
            .cloned()
            .collect();

        let mut devices: Vec<BtDevice> = parse_devices(&all)
            .into_iter()
            .filter(|(mac, name)| !name.is_empty() && name.replace('-', ":") != *mac)
            .map(|(mac, name)| BtDevice {
                paired: paired.contains(&mac),
                connected: connected.contains(&mac),
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

        let mut shared = self.lock();
        let was: Vec<BtDevice> = shared.devices.iter().filter(|d| d.connected).cloned().collect();
        let mut events = Vec::new();
        for d in devices.iter().filter(|d| d.connected) {
            if !was.iter().any(|w| w.mac == d.mac) {
                events.push(BtEvent::Connected(d.name.clone()));
            }
        }
        for w in &was {
            if !devices.iter().any(|d| d.connected && d.mac == w.mac) {
                events.push(BtEvent::Disconnected(w.name.clone()));
            }
        }
        shared.events.extend(events);
        shared.devices = devices;
        shared.last_refresh = Some(Instant::now());
    }
}

// ---------------------------------------------------------------------------
// One-shot commands
// ---------------------------------------------------------------------------

/// Run `bluetoothctl args...` and return its output, killing it after
/// `max_secs` (e.g. if bluetoothd is down it waits forever).
///
/// Don't use bluetoothctl's own `--timeout` for normal commands: it always
/// waits the full time, even when the command finished instantly.
fn ctl(args: &[&str], max_secs: u64) -> Result<String, String> {
    let mut child = Command::new("bluetoothctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "bluetoothctl not installed".to_string(),
            _ => format!("Bluetooth unavailable: {e}"),
        })?;

    let mut stdout = child.stdout.take().ok_or("Bluetooth unavailable")?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });

    let deadline = Instant::now() + Duration::from_secs(max_secs);
    let mut timed_out = false;
    while !matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let bytes = reader.join().unwrap_or_default();

    let out = strip_ansi(&String::from_utf8_lossy(&bytes));
    if timed_out && out.contains("Waiting to connect to bluetoothd") {
        return Err("bluetoothd not running".into());
    }
    if out.contains("No default controller available") {
        return Err("No Bluetooth adapter".into());
    }
    Ok(out)
}

fn power_on() -> Result<(), String> {
    let out = ctl(&["power", "on"], 5)?;
    if out.contains("Failed to set power on") {
        return Err(friendly_error(&out));
    }
    Ok(())
}

/// (major, minor) of the installed BlueZ, e.g. (5, 66).
fn bluez_version() -> (u32, u32) {
    static VERSION: OnceLock<(u32, u32)> = OnceLock::new();
    *VERSION.get_or_init(|| {
        let out = Command::new("bluetoothctl")
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let version = out.rsplit(' ').next().unwrap_or("").trim();
        let mut parts = version.split('.').map(|p| p.parse().unwrap_or(0));
        (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
    })
}

/// MACs of paired devices. `devices Paired` replaced `paired-devices` in
/// BlueZ 5.65; older versions ignore the filter and list everything.
fn paired_macs() -> Vec<String> {
    let args: &[&str] = if bluez_version() >= (5, 65) {
        &["devices", "Paired"]
    } else {
        &["paired-devices"]
    };
    parse_devices(&ctl(args, 5).unwrap_or_default())
        .into_iter()
        .map(|(mac, _)| mac)
        .collect()
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

        if mac.len() == 17 && mac.matches(':').count() == 5 && !devices.iter().any(|(m, _)| m == mac) {
            devices.push((mac.to_string(), name.to_string()));
        }
    }

    devices
}

/// Remove terminal colour / cursor codes and carriage returns.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    // CSI: parameters, then one final byte in @..~
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
            }
            '\r' => out.push('\n'),
            _ => out.push(c),
        }
    }
    out
}

/// Short, screen-sized message for a BlueZ error line.
fn friendly_error(line: &str) -> String {
    const KNOWN: &[(&str, &str)] = &[
        ("profile-unavailable", "No BT audio service"),
        ("page-timeout", "Device not responding"),
        ("ConnectionAttemptFailed", "Device not responding"),
        ("host-down", "Device not responding"),
        ("AuthenticationFailed", "Pairing rejected"),
        ("AuthenticationRejected", "Pairing rejected"),
        ("AuthenticationCanceled", "Pairing cancelled"),
        ("AuthenticationTimeout", "Pairing timed out"),
        ("not available", "Not found, scan again"),
        ("InProgress", "Busy, try again"),
        ("Blocked", "Bluetooth blocked"),
        ("rfkill", "Bluetooth blocked"),
        ("NotReady", "Bluetooth is off"),
        ("No default controller", "No Bluetooth adapter"),
    ];
    if let Some((_, msg)) = KNOWN.iter().find(|(key, _)| line.contains(key)) {
        return msg.to_string();
    }
    // "Failed to connect: org.bluez.Error.Foo bar" -> "Foo bar"
    let detail = line.rsplit("Error.").next().unwrap_or(line).trim();
    detail.chars().take(22).collect()
}

// ---------------------------------------------------------------------------
// Interactive session (pair / connect / disconnect)
// ---------------------------------------------------------------------------

struct Session {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    /// Output not consumed by a `wait_for` yet, colour codes removed.
    buf: String,
}

impl Session {
    fn open() -> Result<Self, String> {
        let mut child = Command::new("bluetoothctl")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => "bluetoothctl not installed".to_string(),
                _ => format!("Bluetooth unavailable: {e}"),
            })?;

        let stdin = child.stdin.take().ok_or("Bluetooth unavailable")?;
        let mut stdout = child.stdout.take().ok_or("Bluetooth unavailable")?;

        // Prompts like "(yes/no):" don't end in a newline, so read raw chunks.
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut chunk) {
                if n == 0 || tx.send(String::from_utf8_lossy(&chunk[..n]).into_owned()).is_err() {
                    break;
                }
            }
        });

        let mut session = Self {
            child,
            stdin,
            rx,
            buf: String::new(),
        };

        // bluetoothctl registers its agent once it reaches bluetoothd.
        session
            .wait_for(
                &["Agent registered", "Agent is already registered"],
                &["No default controller", "Failed to register agent"],
                Duration::from_secs(5),
            )
            .map_err(|e| {
                if e == "timed out" {
                    "bluetoothd not running".to_string()
                } else {
                    friendly_error(&e)
                }
            })?;
        session.send("default-agent");
        let _ = session.wait_for(&["Default agent request successful"], &["Failed"], Duration::from_secs(2));

        Ok(session)
    }

    /// Send a command. Leftover output from earlier commands is dropped
    /// first, so it can't be mistaken for this command's reply.
    fn send(&mut self, command: &str) {
        thread::sleep(Duration::from_millis(30));
        while let Ok(chunk) = self.rx.try_recv() {
            drop(chunk);
        }
        self.buf.clear();
        self.send_raw(command);
    }

    /// Send without clearing (answers to prompts mid-command).
    fn send_raw(&mut self, command: &str) {
        let _ = writeln!(self.stdin, "{command}");
        let _ = self.stdin.flush();
    }

    /// Wait until a line containing one of `ok` or `fail` shows up. Returns
    /// that line (Ok or Err). Agent prompts are answered while waiting.
    fn wait_for(&mut self, ok: &[&str], fail: &[&str], timeout: Duration) -> Result<String, String> {
        let deadline = Instant::now() + timeout;
        loop {
            self.answer_prompts();

            let found = ok
                .iter()
                .chain(fail)
                .filter_map(|p| self.buf.find(p))
                .min();
            if let Some(pos) = found {
                let start = self.buf[..pos].rfind('\n').map_or(0, |i| i + 1);
                let end = self.buf[pos..].find('\n').map_or(self.buf.len(), |i| pos + i);
                let line = self.buf[start..end].trim().to_string();
                self.buf.drain(..end);
                // A line can match both, e.g. "Failed to pair: ...AlreadyExists".
                return if ok.iter().any(|p| line.contains(p)) {
                    Ok(line)
                } else {
                    Err(line)
                };
            }

            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("timed out".into());
            }
            match self.rx.recv_timeout(left) {
                Ok(chunk) => self.buf.push_str(&strip_ansi(&chunk)),
                Err(RecvTimeoutError::Timeout) => return Err("timed out".into()),
                Err(RecvTimeoutError::Disconnected) => return Err("bluetoothctl exited".into()),
            }
        }
    }

    /// Accept pairing / service prompts. Headphones normally pair without
    /// any, but some ask to confirm a passkey or authorise a profile.
    fn answer_prompts(&mut self) {
        const PROMPTS: &[(&str, &str)] = &[
            ("(yes/no):", "yes"),
            ("Enter PIN code:", "0000"),
        ];
        for (prompt, answer) in PROMPTS {
            if let Some(pos) = self.buf.find(prompt) {
                self.buf.replace_range(pos..pos + prompt.len(), "");
                self.send_raw(answer);
            }
        }
    }

    fn disconnect(&mut self, bt: &Bluetooth, device: &BtDevice) -> Result<(), String> {
        bt.set_status(format!("Disconnecting {}...", device.name));
        self.send(&format!("disconnect {}", device.mac));
        self.wait_for(
            // Wording changed between BlueZ versions.
            &["Disconnection successful", "Successful disconnected"],
            &["Failed to disconnect", "not available"],
            Duration::from_secs(10),
        )
        .map_err(|e| friendly_error(&e))?;
        bt.set_status(format!("Disconnected {}", device.name));
        Ok(())
    }

    fn pair_and_connect(&mut self, bt: &Bluetooth, device: &BtDevice) -> Result<(), String> {
        let mac = &device.mac;

        self.send("power on");
        self.wait_for(&["power on succeeded"], &["Failed to set power on"], Duration::from_secs(5))
            .map_err(|e| friendly_error(&e))?;

        if !device.paired {
            // BlueZ forgets unpaired devices shortly after a scan ends. If
            // it has, scan again in this session until it shows up.
            self.send(&format!("info {mac}"));
            let known = self.wait_for(&["Name:", "Alias:", "Paired:"], &["not available"], Duration::from_secs(3));
            if known.is_err() {
                bt.set_status(format!("Looking for {}...", device.name));
                self.send("scan on");
                let seen = self.wait_for(&[mac.as_str()], &[], Duration::from_secs(15));
                if seen.is_err() {
                    self.send("scan off");
                    return Err("Not found. Pairing mode?".into());
                }
            }

            bt.set_status(format!("Pairing {}...", device.name));
            self.send(&format!("pair {mac}"));
            let paired = self.wait_for(
                &["Pairing successful", "AlreadyExists", "Already Exists"],
                &["Failed to pair", "not available"],
                Duration::from_secs(30),
            );
            self.send("scan off");
            paired.map_err(|e| {
                if e == "timed out" { "Pairing timed out".into() } else { friendly_error(&e) }
            })?;
        }

        // Trusted devices reconnect on their own after a reboot.
        self.send(&format!("trust {mac}"));
        let _ = self.wait_for(&["trust succeeded"], &["Failed to set trusted", "not available"], Duration::from_secs(5));

        bt.set_status(format!("Connecting {}...", device.name));
        self.send(&format!("connect {mac}"));
        self.wait_for(
            &["Connection successful"],
            &["Failed to connect", "not available"],
            Duration::from_secs(20),
        )
        .map_err(|e| {
            if e == "timed out" { "Connect timed out".into() } else { friendly_error(&e) }
        })?;

        bt.set_status(format!("Connected {}", device.name));
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.send("quit");
        // Give it a moment to exit cleanly, then make sure it's gone.
        for _ in 0..10 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
