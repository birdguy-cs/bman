//! Bluetooth through BlueZ over D-Bus (the `zbus` crate, pure Rust).
//!
//! Only needs `bluetoothd` running and the system D-Bus, which every Linux
//! distro with Bluetooth has, whatever the init system. No `bluetoothctl`,
//! no libdbus.
//!
//! Every slow operation runs on a background thread so the UI never blocks.
//! Audio goes to the system's default output, which PipeWire / PulseAudio
//! switch to the headphones when they connect. The "Wireless" output uses it.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use zbus::{
    blocking::{Connection, Proxy},
    zvariant::{OwnedObjectPath, OwnedValue},
};

/// How long a scan listens for devices.
const SCAN_SECONDS: u64 = 8;
/// How long to look for a device BlueZ has forgotten before pairing it.
const FIND_SECONDS: u64 = 15;
/// How often the connection state is re-checked in the background, so
/// headphones turning off (or reconnecting on their own) are noticed.
const POLL_SLOW: Duration = Duration::from_secs(10);
/// Re-check interval while a Bluetooth / settings screen is open.
const POLL_FAST: Duration = Duration::from_secs(3);

const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const AGENT_PATH: &str = "/bman/agent";

#[derive(Clone, Debug)]
pub struct BtDevice {
    pub mac: String,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
    /// D-Bus object path, e.g. /org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF.
    path: String,
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
    last_refresh: Option<Instant>,
    /// The system bus connection, opened on first use and reopened if it fails.
    conn: Option<Connection>,
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
            self.run_job(None, |bt| {
                if let Err(e) = bt.update_devices() {
                    bt.set_status(e);
                }
            });
        }
    }

    /// Re-read the device list in the background.
    pub fn refresh(&self) {
        self.run_job(Some("Checking..."), |bt| match bt.update_devices() {
            Ok(()) if bt.status() == "Checking..." => bt.set_status(""),
            Ok(()) => {}
            Err(e) => bt.set_status(e),
        });
    }

    /// Switch the adapter on (e.g. right after it was unblocked) and re-read
    /// the device list.
    pub fn power_up(&self) {
        self.run_job(Some("Turning on..."), |bt| {
            let result = bt.connection().and_then(|conn| power_on(&conn));
            match result.and_then(|_| bt.update_devices()) {
                Ok(()) => bt.set_status(""),
                Err(e) => bt.set_status(e),
            }
        });
    }

    /// Search for nearby devices for a few seconds, then update the list.
    pub fn scan(&self) {
        self.run_job(Some("Scanning..."), |bt| {
            let result = (|| {
                let conn = bt.connection()?;
                let adapter = power_on(&conn)?;
                let proxy = proxy(&conn, &adapter, ADAPTER)?;
                proxy.call_method("StartDiscovery", &()).map_err(friendly)?;

                // Show devices as they're found.
                let end = Instant::now() + Duration::from_secs(SCAN_SECONDS);
                while Instant::now() < end {
                    thread::sleep(Duration::from_secs(1));
                    let _ = bt.update_devices();
                }
                let _ = proxy.call_method("StopDiscovery", &());
                bt.update_devices()
            })();

            match result {
                Ok(()) => {
                    let count = bt.lock().devices.len();
                    bt.set_status(format!("Found {count} devices"));
                }
                Err(e) => bt.set_status(e),
            }
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
            let result = if device.connected {
                bt.disconnect(&device)
            } else {
                bt.pair_and_connect(&device)
            };
            if let Err(e) = result {
                bt.set_status(e);
            }
            let _ = bt.update_devices();
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
            let mut shared = bt.lock();
            shared.busy = false;
            shared.last_refresh = Some(Instant::now());
        });
    }

    /// The system bus, connected on first use (and again after an error, in
    /// case D-Bus or bluetoothd was restarted).
    fn connection(&self) -> Result<Connection, String> {
        if let Some(conn) = self.lock().conn.clone() {
            return Ok(conn);
        }
        let conn = Connection::system().map_err(|_| "No system D-Bus".to_string())?;
        self.lock().conn = Some(conn.clone());
        Ok(conn)
    }

    fn update_devices(&self) -> Result<(), String> {
        let conn = self.connection()?;
        let objects = match managed_objects(&conn) {
            Ok(objects) => objects,
            Err(e) => {
                self.lock().conn = None; // reconnect next time
                return Err(e);
            }
        };
        if !objects.values().any(|ifaces| ifaces.contains_key(ADAPTER)) {
            return Err("No Bluetooth adapter".into());
        }

        let mut devices: Vec<BtDevice> = objects
            .iter()
            .filter_map(|(path, ifaces)| {
                let props = ifaces.get(DEVICE)?;
                let mac: String = prop(props, "Address")?;
                // Unnamed devices (random BLE beacons etc.) aren't useful here.
                let name: String = prop(props, "Name").or_else(|| prop(props, "Alias"))?;
                if name.replace('-', ":") == mac {
                    return None;
                }
                Some(BtDevice {
                    paired: prop(props, "Paired").unwrap_or(false),
                    connected: prop(props, "Connected").unwrap_or(false),
                    mac,
                    name,
                    path: path.to_string(),
                })
            })
            .collect();

        devices.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then(b.paired.cmp(&a.paired))
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        let mut shared = self.lock();
        let mut events = Vec::new();
        for d in devices.iter().filter(|d| d.connected) {
            if !shared.devices.iter().any(|w| w.connected && w.mac == d.mac) {
                events.push(BtEvent::Connected(d.name.clone()));
            }
        }
        for w in shared.devices.iter().filter(|w| w.connected) {
            if !devices.iter().any(|d| d.connected && d.mac == w.mac) {
                events.push(BtEvent::Disconnected(w.name.clone()));
            }
        }
        shared.events.extend(events);
        shared.devices = devices;
        shared.last_refresh = Some(Instant::now());
        Ok(())
    }

    fn disconnect(&self, device: &BtDevice) -> Result<(), String> {
        let conn = self.connection()?;
        self.set_status(format!("Disconnecting {}...", device.name));
        proxy(&conn, &device.path, DEVICE)?
            .call_method("Disconnect", &())
            .map_err(friendly)?;
        self.set_status(format!("Disconnected {}", device.name));
        Ok(())
    }

    fn pair_and_connect(&self, device: &BtDevice) -> Result<(), String> {
        let conn = self.connection()?;
        let adapter = power_on(&conn)?;
        register_agent(&conn);

        if !device.paired {
            // BlueZ forgets unpaired devices shortly after a scan ends. If
            // it has, scan again until it shows up.
            let mut discovering = false;
            if !device_exists(&conn, &device.path) {
                self.set_status(format!("Looking for {}...", device.name));
                proxy(&conn, &adapter, ADAPTER)?
                    .call_method("StartDiscovery", &())
                    .map_err(friendly)?;
                discovering = true;

                let end = Instant::now() + Duration::from_secs(FIND_SECONDS);
                while !device_exists(&conn, &device.path) {
                    if Instant::now() >= end {
                        stop_discovery(&conn, &adapter);
                        return Err("Not found. Pairing mode?".into());
                    }
                    thread::sleep(Duration::from_millis(500));
                }
            }

            self.set_status(format!("Pairing {}...", device.name));
            let result = proxy(&conn, &device.path, DEVICE)?.call_method("Pair", &());
            if discovering {
                stop_discovery(&conn, &adapter);
            }
            match result {
                Ok(_) => {}
                Err(e) if error_text(&e).contains("AlreadyExists") => {}
                Err(e) => return Err(friendly(e)),
            }
        }

        // Trusted devices reconnect on their own after a reboot.
        let dev = proxy(&conn, &device.path, DEVICE)?;
        let _ = dev.set_property("Trusted", true);

        self.set_status(format!("Connecting {}...", device.name));
        dev.call_method("Connect", &()).map_err(friendly)?;
        self.set_status(format!("Connected {}", device.name));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// D-Bus helpers
// ---------------------------------------------------------------------------

type Interfaces = HashMap<String, HashMap<String, OwnedValue>>;

fn proxy<'a>(conn: &Connection, path: &'a str, iface: &'a str) -> Result<Proxy<'a>, String> {
    Proxy::new(conn, BLUEZ, path.to_string(), iface).map_err(friendly)
}

/// Every BlueZ object (adapters, devices) with its properties, in one call.
fn managed_objects(conn: &Connection) -> Result<HashMap<OwnedObjectPath, Interfaces>, String> {
    let manager = Proxy::new(conn, BLUEZ, "/", "org.freedesktop.DBus.ObjectManager").map_err(friendly)?;
    manager
        .call("GetManagedObjects", &())
        .map_err(friendly)
}

fn prop<T: TryFrom<OwnedValue>>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    props.get(key)?.try_clone().ok()?.try_into().ok()
}

fn device_exists(conn: &Connection, path: &str) -> bool {
    managed_objects(conn).is_ok_and(|objects| {
        objects
            .iter()
            .any(|(p, ifaces)| p.as_str() == path && ifaces.contains_key(DEVICE))
    })
}

/// The first adapter's path (e.g. /org/bluez/hci0), switched on.
fn power_on(conn: &Connection) -> Result<String, String> {
    let objects = managed_objects(conn)?;
    let mut adapters: Vec<&str> = objects
        .iter()
        .filter(|(_, ifaces)| ifaces.contains_key(ADAPTER))
        .map(|(path, _)| path.as_str())
        .collect();
    adapters.sort();
    let adapter = adapters.first().ok_or("No Bluetooth adapter")?.to_string();

    {
        let proxy = proxy(conn, &adapter, ADAPTER)?;
        if !proxy.get_property::<bool>("Powered").unwrap_or(false) {
            proxy.set_property("Powered", true).map_err(|e| friendly(e.into()))?;
        }
    }
    Ok(adapter)
}

fn stop_discovery(conn: &Connection, adapter: &str) {
    if let Ok(proxy) = proxy(conn, adapter, ADAPTER) {
        let _ = proxy.call_method("StopDiscovery", &());
    }
}

/// Register our pairing agent (accepts everything, like headphones expect).
/// BlueZ uses the agent of whoever calls Pair, so this works even when a
/// desktop's own agent is the default. Safe to call repeatedly.
fn register_agent(conn: &Connection) {
    let _ = conn.object_server().at(AGENT_PATH, Agent);
    if let Ok(manager) = Proxy::new(conn, BLUEZ, "/org/bluez", "org.bluez.AgentManager1") {
        let path = zbus::zvariant::ObjectPath::from_static_str_unchecked(AGENT_PATH);
        let _ = manager.call_method("RegisterAgent", &(&path, "NoInputNoOutput"));
        let _ = manager.call_method("RequestDefaultAgent", &(&path,));
    }
}

/// org.bluez.Agent1: answers pairing requests without user input.
struct Agent;

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}
    fn request_pin_code(&self, _device: OwnedObjectPath) -> String {
        "0000".into()
    }
    fn display_pin_code(&self, _device: OwnedObjectPath, _pincode: String) {}
    fn request_passkey(&self, _device: OwnedObjectPath) -> u32 {
        0
    }
    fn display_passkey(&self, _device: OwnedObjectPath, _passkey: u32, _entered: u16) {}
    fn request_confirmation(&self, _device: OwnedObjectPath, _passkey: u32) {}
    fn request_authorization(&self, _device: OwnedObjectPath) {}
    fn authorize_service(&self, _device: OwnedObjectPath, _uuid: String) {}
    fn cancel(&self) {}
}

/// "org.bluez.Error.Failed br-connection-profile-unavailable" style text.
fn error_text(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, msg, _) => {
            format!("{} {}", name.as_str(), msg.as_deref().unwrap_or(""))
        }
        other => other.to_string(),
    }
}

/// Short, screen-sized message for a D-Bus / BlueZ error.
fn friendly(e: zbus::Error) -> String {
    let text = error_text(&e);
    const KNOWN: &[(&str, &str)] = &[
        ("ServiceUnknown", "bluetoothd not running"),
        ("NameHasNoOwner", "bluetoothd not running"),
        ("AccessDenied", "No permission for BT"),
        ("profile-unavailable", "No BT audio service"),
        ("page-timeout", "Device not responding"),
        ("ConnectionAttemptFailed", "Device not responding"),
        ("host-down", "Device not responding"),
        ("AuthenticationFailed", "Pairing rejected"),
        ("AuthenticationRejected", "Pairing rejected"),
        ("AuthenticationCanceled", "Pairing cancelled"),
        ("AuthenticationTimeout", "Pairing timed out"),
        ("UnknownObject", "Not found, scan again"),
        ("DoesNotExist", "Not found, scan again"),
        ("InProgress", "Busy, try again"),
        ("Blocked", "Bluetooth blocked"),
        ("rfkill", "Bluetooth blocked"),
        ("NotReady", "Bluetooth is off"),
        ("NoReply", "Device not responding"),
        ("Timeout", "Device not responding"),
    ];
    if let Some((_, msg)) = KNOWN.iter().find(|(key, _)| text.contains(key)) {
        return msg.to_string();
    }
    // "org.bluez.Error.Foo bar" -> "Foo bar"
    let detail = text.rsplit("Error.").next().unwrap_or(&text).trim();
    detail.chars().take(22).collect()
}
