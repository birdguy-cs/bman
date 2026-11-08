//! Wi-Fi through NetworkManager over D-Bus (`zbus`, pure Rust).
//!
//! NetworkManager is the default on Raspberry Pi OS (Bookworm and later),
//! Debian/Ubuntu/Pop!_OS desktops, Fedora, Manjaro and SteamOS. Systems
//! without it show "No network manager". No external commands are used.
//!
//! Every slow operation runs on a background thread so the UI never blocks.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use zbus::{
    blocking::{Connection, Proxy},
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
const CONNECTION: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const AP: &str = "org.freedesktop.NetworkManager.AccessPoint";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const IP4: &str = "org.freedesktop.NetworkManager.IP4Config";

const DEVICE_TYPE_WIFI: u32 = 2;
/// Re-check interval while the Network screen is open.
const POLL: Duration = Duration::from_secs(3);
/// How long a connection attempt may take before it counts as failed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct WifiNetwork {
    pub ssid: String,
    /// Signal strength 0-100.
    pub strength: u8,
    /// Needs a password.
    pub secured: bool,
    /// A saved NetworkManager connection exists for it.
    pub saved: bool,
    pub connected: bool,
    /// Access point object path (for connecting to a new network).
    ap: String,
}

#[derive(Default)]
struct Shared {
    networks: Vec<WifiNetwork>,
    wifi_enabled: bool,
    ip: Option<String>,
    status: String,
    busy: bool,
    last_refresh: Option<Instant>,
    conn: Option<Connection>,
}

#[derive(Clone)]
pub struct Network {
    shared: Arc<Mutex<Shared>>,
}

impl Network {
    pub fn new() -> Self {
        let net = Self {
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        net.refresh();
        net
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_status(&self, status: impl Into<String>) {
        self.lock().status = status.into();
    }

    /// Visible networks: connected first, then saved, then by signal.
    pub fn networks(&self) -> Vec<WifiNetwork> {
        self.lock().networks.clone()
    }

    pub fn wifi_enabled(&self) -> bool {
        self.lock().wifi_enabled
    }

    /// IPv4 address of whichever interface has one (Wi-Fi first).
    pub fn ip(&self) -> Option<String> {
        self.lock().ip.clone()
    }

    pub fn connected_ssid(&self) -> Option<String> {
        self.lock()
            .networks
            .iter()
            .find(|n| n.connected)
            .map(|n| n.ssid.clone())
    }

    pub fn status(&self) -> String {
        self.lock().status.clone()
    }

    pub fn is_busy(&self) -> bool {
        self.lock().busy
    }

    /// Call every frame while the Network screen is open.
    pub fn poll(&self) {
        let due = {
            let shared = self.lock();
            !shared.busy && shared.last_refresh.is_some_and(|t| t.elapsed() >= POLL)
        };
        if due {
            self.run_job(None, |net| {
                if let Err(e) = net.update() {
                    net.set_status(e);
                }
            });
        }
    }

    pub fn refresh(&self) {
        self.run_job(Some("Checking..."), |net| match net.update() {
            Ok(()) if net.status() == "Checking..." => net.set_status(""),
            Ok(()) => {}
            Err(e) => net.set_status(e),
        });
    }

    /// Ask NetworkManager to scan, then list what it finds.
    pub fn scan(&self) {
        self.run_job(Some("Scanning..."), |net| {
            let result = (|| {
                let conn = net.connection()?;
                let device = wifi_device(&conn)?;
                let options: HashMap<&str, Value> = HashMap::new();
                // Fails if a scan ran very recently; the list is fresh then anyway.
                let _ = proxy(&conn, &device, WIRELESS)?.call_method("RequestScan", &(options,));
                for _ in 0..4 {
                    thread::sleep(Duration::from_secs(1));
                    let _ = net.update();
                }
                net.update()
            })();
            match result {
                Ok(()) => {
                    let count = net.lock().networks.len();
                    net.set_status(format!("Found {count} networks"));
                }
                Err(e) => net.set_status(e),
            }
        });
    }

    pub fn set_wifi_enabled(&self, on: bool) {
        self.run_job(Some(if on { "Turning on..." } else { "Turning off..." }), move |net| {
            let result = net
                .connection()
                .and_then(|conn| proxy(&conn, NM_PATH, NM))
                .and_then(|nm| nm.set_property("WirelessEnabled", on).map_err(|e| friendly(e.into())));
            match result {
                Ok(()) => net.set_status(if on { "Wi-Fi on" } else { "Wi-Fi off" }),
                Err(e) => net.set_status(e),
            }
            thread::sleep(Duration::from_millis(500));
            let _ = net.update();
        });
    }

    /// Connected -> disconnect. Saved or open -> connect. A secured network
    /// that isn't saved needs `connect_with_password` instead.
    pub fn activate(&self, ssid: String) {
        let Some(network) = self.lock().networks.iter().find(|n| n.ssid == ssid).cloned() else {
            return;
        };
        self.run_job(Some("Working..."), move |net| {
            let result = if network.connected {
                net.disconnect(&network)
            } else {
                net.connect(&network, None)
            };
            if let Err(e) = result {
                net.set_status(e);
            }
            let _ = net.update();
        });
    }

    pub fn connect_with_password(&self, ssid: String, password: String) {
        let Some(network) = self.lock().networks.iter().find(|n| n.ssid == ssid).cloned() else {
            self.set_status("Network gone, scan again");
            return;
        };
        self.run_job(Some("Working..."), move |net| {
            if let Err(e) = net.connect(&network, Some(&password)) {
                net.set_status(e);
            }
            let _ = net.update();
        });
    }

    /// Delete the saved connection(s) for `ssid`.
    pub fn forget(&self, ssid: String) {
        self.run_job(Some("Forgetting..."), move |net| {
            let result = (|| {
                let conn = net.connection()?;
                let saved = saved_connections(&conn)?;
                let mut found = false;
                for path in saved.get(&ssid).into_iter().flatten() {
                    proxy(&conn, path, CONNECTION)?
                        .call_method("Delete", &())
                        .map_err(friendly)?;
                    found = true;
                }
                Ok::<bool, String>(found)
            })();
            match result {
                Ok(true) => net.set_status(format!("Forgot {ssid}")),
                Ok(false) => net.set_status("Not a saved network"),
                Err(e) => net.set_status(e),
            }
            let _ = net.update();
        });
    }

    /// Save networks from config.txt that NetworkManager doesn't know yet.
    /// NetworkManager connects to saved networks on its own.
    pub fn import(&self, networks: Vec<(String, String)>) {
        if networks.is_empty() {
            return;
        }
        // Runs after the first refresh; retry briefly if that's still going.
        let net = self.clone();
        thread::spawn(move || {
            crate::sched::background(); // never compete with playback
            for _ in 0..50 {
                if !net.is_busy() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            net.run_job(None, move |net| {
                let result = (|| {
                    let conn = net.connection()?;
                    let saved = saved_connections(&conn)?;
                    let settings = proxy(&conn, SETTINGS_PATH, SETTINGS)?;
                    for (ssid, password) in &networks {
                        if saved.contains_key(ssid) {
                            continue;
                        }
                        let mut connection = wifi_settings(Some(ssid), password_option(password), None);
                        connection
                            .get_mut("connection")
                            .unwrap()
                            .insert("id", Value::from(ssid.as_str()));
                        match settings.call_method("AddConnection", &(connection,)) {
                            Ok(_) => info!("Added Wi-Fi network from config: {ssid}"),
                            Err(e) => warn!("Couldn't add Wi-Fi network {ssid}: {}", friendly(e)),
                        }
                    }
                    Ok::<(), String>(())
                })();
                if let Err(e) = result {
                    warn!("Couldn't import Wi-Fi networks: {e}");
                }
                let _ = net.update();
            });
        });
    }

    /// Run `job` on a thread unless another job is already running.
    fn run_job(&self, status: Option<&str>, job: impl FnOnce(&Network) + Send + 'static) {
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
        let net = self.clone();
        thread::spawn(move || {
            crate::sched::background(); // never compete with playback
            job(&net);
            let mut shared = net.lock();
            shared.busy = false;
            shared.last_refresh = Some(Instant::now());
        });
    }

    fn connection(&self) -> Result<Connection, String> {
        if let Some(conn) = self.lock().conn.clone() {
            return Ok(conn);
        }
        let conn = Connection::system().map_err(|_| "No system D-Bus".to_string())?;
        self.lock().conn = Some(conn.clone());
        Ok(conn)
    }

    fn update(&self) -> Result<(), String> {
        let conn = self.connection()?;
        let nm = proxy(&conn, NM_PATH, NM)?;
        let wifi_enabled: bool = match nm.get_property("WirelessEnabled") {
            Ok(on) => on,
            Err(e) => {
                self.lock().conn = None; // reconnect next time
                return Err(friendly(e.into()));
            }
        };

        let ip = current_ip(&conn);
        let mut networks = Vec::new();

        if let Ok(device) = wifi_device(&conn) {
            let wireless = proxy(&conn, &device, WIRELESS)?;
            let active_ap: OwnedObjectPath = wireless
                .get_property("ActiveAccessPoint")
                .unwrap_or_else(|_| OwnedObjectPath::try_from("/").unwrap());
            let aps: Vec<OwnedObjectPath> = wireless
                .call("GetAllAccessPoints", &())
                .unwrap_or_default();
            let saved = saved_connections(&conn).unwrap_or_default();

            for ap_path in aps {
                let Ok(ap) = proxy(&conn, ap_path.as_str(), AP) else {
                    continue;
                };
                let ssid_bytes: Vec<u8> = ap.get_property("Ssid").unwrap_or_default();
                let ssid = String::from_utf8_lossy(&ssid_bytes).trim().to_string();
                if ssid.is_empty() {
                    continue; // hidden network
                }
                let strength: u8 = ap.get_property("Strength").unwrap_or(0);
                let flags: u32 = ap.get_property("Flags").unwrap_or(0);
                let wpa: u32 = ap.get_property("WpaFlags").unwrap_or(0);
                let rsn: u32 = ap.get_property("RsnFlags").unwrap_or(0);
                let connected = ap_path == active_ap;
                // WPA-Enterprise (802.1X) needs a username/certificates: not supported.
                if (wpa | rsn) & 0x200 != 0 && !connected {
                    continue;
                }

                // The same network shows up once per access point / band:
                // keep the strongest (or the connected one).
                if let Some(existing) = networks.iter_mut().find(|n: &&mut WifiNetwork| n.ssid == ssid) {
                    if connected || (!existing.connected && strength > existing.strength) {
                        existing.strength = strength;
                        existing.ap = ap_path.to_string();
                        existing.connected = connected;
                    }
                    continue;
                }

                networks.push(WifiNetwork {
                    saved: saved.contains_key(&ssid),
                    secured: flags & 1 != 0 || wpa != 0 || rsn != 0,
                    strength,
                    connected,
                    ap: ap_path.to_string(),
                    ssid,
                });
            }
        }

        networks.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then(b.saved.cmp(&a.saved))
                .then(b.strength.cmp(&a.strength))
        });

        let mut shared = self.lock();
        shared.wifi_enabled = wifi_enabled;
        shared.ip = ip;
        shared.networks = networks;
        shared.last_refresh = Some(Instant::now());
        Ok(())
    }

    fn disconnect(&self, network: &WifiNetwork) -> Result<(), String> {
        let conn = self.connection()?;
        let device = wifi_device(&conn)?;
        self.set_status(format!("Disconnecting {}...", network.ssid));
        proxy(&conn, &device, DEVICE)?
            .call_method("Disconnect", &())
            .map_err(friendly)?;
        self.set_status(format!("Disconnected {}", network.ssid));
        Ok(())
    }

    fn connect(&self, network: &WifiNetwork, password: Option<&str>) -> Result<(), String> {
        let conn = self.connection()?;
        let device = wifi_device(&conn)?;
        let nm = proxy(&conn, NM_PATH, NM)?;
        let device_path = ObjectPath::try_from(device.as_str()).map_err(|e| e.to_string())?;
        let ap_path = ObjectPath::try_from(network.ap.as_str()).map_err(|e| e.to_string())?;

        self.set_status(format!("Connecting {}...", network.ssid));

        let saved = saved_connections(&conn)?;
        let (active, added): (OwnedObjectPath, Option<OwnedObjectPath>) =
            match (saved.get(&network.ssid).and_then(|v| v.first()), password) {
                // Saved network, no new password: just bring it up.
                (Some(saved_path), None) => {
                    let saved_path = ObjectPath::try_from(saved_path.as_str()).map_err(|e| e.to_string())?;
                    let active = nm
                        .call("ActivateConnection", &(&saved_path, &device_path, &ap_path))
                        .map_err(friendly)?;
                    (active, None)
                }
                _ => {
                    if network.secured && password.is_none() {
                        return Err("Needs a password".into());
                    }
                    // A new password replaces any old saved entry.
                    for path in saved.get(&network.ssid).into_iter().flatten() {
                        if let Ok(p) = proxy(&conn, path, CONNECTION) {
                            let _ = p.call_method("Delete", &());
                        }
                    }
                    let settings = wifi_settings(None, password, Some(network.secured));
                    let (path, active): (OwnedObjectPath, OwnedObjectPath) = nm
                        .call("AddAndActivateConnection", &(settings, &device_path, &ap_path))
                        .map_err(friendly)?;
                    (active, Some(path))
                }
            };

        // Activation carries on in the background; wait for the result.
        match wait_activated(&conn, &active) {
            Ok(()) => {
                self.set_status(format!("Connected {}", network.ssid));
                Ok(())
            }
            Err(e) => {
                // Don't keep a new entry with a wrong password around.
                if let Some(path) = added {
                    if let Ok(p) = proxy(&conn, path.as_str(), CONNECTION) {
                        let _ = p.call_method("Delete", &());
                    }
                }
                Err(if password.is_some() { "Wrong password?".into() } else { e })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// D-Bus helpers
// ---------------------------------------------------------------------------

fn proxy<'a>(conn: &Connection, path: &'a str, iface: &'a str) -> Result<Proxy<'a>, String> {
    Proxy::new(conn, NM, path.to_string(), iface).map_err(friendly)
}

/// The first Wi-Fi device's object path.
fn wifi_device(conn: &Connection) -> Result<String, String> {
    let nm = proxy(conn, NM_PATH, NM)?;
    let devices: Vec<OwnedObjectPath> = nm.call("GetDevices", &()).map_err(friendly)?;
    for path in devices {
        if let Ok(dev) = proxy(conn, path.as_str(), DEVICE) {
            if dev.get_property::<u32>("DeviceType").ok() == Some(DEVICE_TYPE_WIFI) {
                return Ok(path.to_string());
            }
        }
    }
    Err("No Wi-Fi adapter".into())
}

/// First IPv4 address found, Wi-Fi devices first.
fn current_ip(conn: &Connection) -> Option<String> {
    let nm = proxy(conn, NM_PATH, NM).ok()?;
    let devices: Vec<OwnedObjectPath> = nm.call("GetDevices", &()).ok()?;
    let mut found: Vec<(bool, String)> = Vec::new();
    for path in devices {
        let Ok(dev) = proxy(conn, path.as_str(), DEVICE) else {
            continue;
        };
        let Ok(config) = dev.get_property::<OwnedObjectPath>("Ip4Config") else {
            continue;
        };
        if config.as_str() == "/" {
            continue;
        }
        let Ok(ip4) = proxy(conn, config.as_str(), IP4) else {
            continue;
        };
        let data: Vec<HashMap<String, OwnedValue>> = ip4.get_property("AddressData").unwrap_or_default();
        let address = data
            .iter()
            .filter_map(|entry| String::try_from(entry.get("address")?.try_clone().ok()?).ok())
            .find(|a| !a.starts_with("127."));
        if let Some(address) = address {
            let is_wifi = dev.get_property::<u32>("DeviceType").ok() == Some(DEVICE_TYPE_WIFI);
            found.push((is_wifi, address));
        }
    }
    found.sort_by_key(|(is_wifi, _)| !is_wifi);
    found.into_iter().next().map(|(_, ip)| ip)
}

/// Saved Wi-Fi connections by SSID.
fn saved_connections(conn: &Connection) -> Result<HashMap<String, Vec<String>>, String> {
    let settings = proxy(conn, SETTINGS_PATH, SETTINGS)?;
    let paths: Vec<OwnedObjectPath> = settings.call("ListConnections", &()).map_err(friendly)?;
    let mut by_ssid: HashMap<String, Vec<String>> = HashMap::new();
    for path in paths {
        let Ok(c) = proxy(conn, path.as_str(), CONNECTION) else {
            continue;
        };
        let Ok(s) = c.call::<_, _, HashMap<String, HashMap<String, OwnedValue>>>("GetSettings", &()) else {
            continue;
        };
        let Some(wifi) = s.get("802-11-wireless") else {
            continue;
        };
        let Some(ssid) = wifi
            .get("ssid")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| Vec::<u8>::try_from(v).ok())
        else {
            continue;
        };
        let ssid = String::from_utf8_lossy(&ssid).trim().to_string();
        by_ssid.entry(ssid).or_default().push(path.to_string());
    }
    Ok(by_ssid)
}

fn password_option(password: &str) -> Option<&str> {
    (!password.is_empty()).then_some(password)
}

/// Connection settings for NetworkManager. `ssid` is only needed when no
/// access point is given (saving a network from config.txt).
fn wifi_settings<'a>(
    ssid: Option<&'a str>,
    password: Option<&'a str>,
    secured: Option<bool>,
) -> HashMap<&'static str, HashMap<&'static str, Value<'a>>> {
    let mut settings: HashMap<&'static str, HashMap<&'static str, Value<'a>>> = HashMap::new();
    let mut connection = HashMap::new();
    connection.insert("type", Value::from("802-11-wireless"));
    settings.insert("connection", connection);

    let mut wifi = HashMap::new();
    if let Some(ssid) = ssid {
        wifi.insert("ssid", Value::from(ssid.as_bytes().to_vec()));
        wifi.insert("mode", Value::from("infrastructure"));
    }
    settings.insert("802-11-wireless", wifi);

    // WPA/WPA2 personal. NetworkManager upgrades to WPA3 (SAE) when the
    // access point needs it.
    if let Some(password) = password.filter(|_| secured != Some(false)) {
        let mut security = HashMap::new();
        security.insert("key-mgmt", Value::from("wpa-psk"));
        security.insert("psk", Value::from(password));
        settings.insert("802-11-wireless-security", security);
    }
    settings
}

/// Wait until an active connection is up (state 2) or has failed.
fn wait_activated(conn: &Connection, active: &OwnedObjectPath) -> Result<(), String> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    while Instant::now() < deadline {
        let state = proxy(conn, active.as_str(), ACTIVE)
            .ok()
            .and_then(|p| p.get_property::<u32>("State").ok());
        match state {
            Some(2) => return Ok(()),
            Some(3) | Some(4) | None => return Err("Couldn't connect".into()),
            _ => thread::sleep(Duration::from_millis(500)),
        }
    }
    Err("Connect timed out".into())
}

/// Short, screen-sized message for a D-Bus / NetworkManager error.
fn friendly(e: zbus::Error) -> String {
    let text = match &e {
        zbus::Error::MethodError(name, msg, _) => {
            format!("{} {}", name.as_str(), msg.as_deref().unwrap_or(""))
        }
        other => other.to_string(),
    };
    const KNOWN: &[(&str, &str)] = &[
        ("ServiceUnknown", "No network manager"),
        ("NameHasNoOwner", "No network manager"),
        ("PermissionDenied", "No permission"),
        ("AccessDenied", "No permission"),
        ("NotAuthorized", "No permission"),
        ("UnknownConnection", "Network gone, scan again"),
        ("UnknownDevice", "No Wi-Fi adapter"),
        ("ConnectionActivating", "Busy, try again"),
        ("InvalidProperty", "Not supported"),
    ];
    if let Some((_, msg)) = KNOWN.iter().find(|(key, _)| text.contains(key)) {
        return msg.to_string();
    }
    let detail = text.rsplit("Error.").next().unwrap_or(&text).trim();
    detail.chars().take(22).collect()
}
