//! USB transfer: the Pi's USB data port acts as a device, so a PC sees bman
//! like a phone or music player (MTP, see mtp.rs) with a Music and a Videos
//! folder. No disk image: those are bman's real folders, and bman keeps
//! playing while files are copied.
//!
//! Plain kernel interfaces, no external programs: a USB gadget made in
//! configfs (`/sys/kernel/config/usb_gadget/bman`) with one FunctionFS
//! function mounted at `/run/bman-mtp`. Making those needs root, so it's the
//! root helper `bman --usb on --uid U --gid G` (root.rs), which also chowns
//! the mount and the gadget's UDC file to the user. bman then writes the
//! USB descriptors to `ep0`, binds the gadget to the port (UDC) itself and
//! serves MTP on `ep1` (bulk IN) / `ep2` (bulk OUT). configfs is gone after a
//! reboot, so the helper runs again at startup (`sudo -n`, silently).
//!
//! A Raspberry Pi's port is a USB host until `dtoverlay=dwc2,dr_mode=peripheral`
//! is in config.txt; the helper adds it and asks for a reboot when there's no
//! UDC yet.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use crate::mtp::{self, Mtp, Transport};
use crate::root::{RootError, euid, run_helper};

const CONFIGFS: &str = "/sys/kernel/config";
const GADGET: &str = "/sys/kernel/config/usb_gadget/bman";
/// FunctionFS instance name: the function is `ffs.mtp`, mounted as "mtp".
const FFS_NAME: &str = "mtp";
const FFS_DIR: &str = "/run/bman-mtp";
const UDC_CLASS: &str = "/sys/class/udc";
const OVERLAY: &str = "dtoverlay=dwc2,dr_mode=peripheral";

// FunctionFS (linux/usb/functionfs.h)
const DESCRIPTORS_MAGIC_V2: u32 = 3;
const STRINGS_MAGIC: u32 = 2;
const HAS_FS_DESC: u32 = 1;
const HAS_HS_DESC: u32 = 2;
const EVENT_SIZE: usize = 12;
const EV_BIND: u8 = 0;
const EV_UNBIND: u8 = 1;
const EV_ENABLE: u8 = 2;
const EV_DISABLE: u8 = 3;
const EV_SETUP: u8 = 4;
const EV_SUSPEND: u8 = 5;

// MTP class requests on ep0
const REQ_CANCEL: u8 = 0x64;
const REQ_DEVICE_RESET: u8 = 0x66;
const REQ_GET_DEVICE_STATUS: u8 = 0x67;

/// The port, once set up; lives as long as bman (closing ep0 would undo the
/// descriptors, so switching off just unbinds the gadget).
pub struct Usb {
    udc: String,
    enabled: bool,
    connected: Arc<AtomicBool>,
    status: mtp::Status,
}

impl Usb {
    /// Write the descriptors, start serving, and bind to the port.
    /// `folders`: (name shown, folder on disk).
    pub fn start(folders: Vec<(&'static str, PathBuf)>, status: mtp::Status) -> Result<Self, String> {
        let udc = udc_name().ok_or("No USB device port")?;
        let mut ep0 = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(Path::new(FFS_DIR).join("ep0"))
            .map_err(|e| format!("USB: {e}"))?;
        ep0.write_all(&descriptors()).map_err(|e| format!("USB descriptors: {e}"))?;
        ep0.write_all(&strings()).map_err(|e| format!("USB strings: {e}"))?;

        let connected = Arc::new(AtomicBool::new(false));
        let max_packet = Arc::new(AtomicUsize::new(512));
        {
            let (connected, max_packet, status, udc) =
                (connected.clone(), max_packet.clone(), status.clone(), udc.clone());
            thread::Builder::new()
                .name("bman-usb-ep0".into())
                .spawn(move || control_loop(ep0, &udc, &connected, &max_packet, &status))
                .map_err(|e| e.to_string())?;
        }
        {
            let status = status.clone();
            thread::Builder::new()
                .name("bman-mtp".into())
                .spawn(move || {
                    crate::sched::background(); // copying never competes with playback
                    let mut mtp = Mtp::new("bman", crate::update::VERSION, &folders, status);
                    data_loop(&mut mtp, &max_packet);
                })
                .map_err(|e| e.to_string())?;
        }
        let mut usb = Self { udc, enabled: false, connected, status };
        usb.set_enabled(true)?;
        Ok(usb)
    }

    /// Bind to / unbind from the port (the PC sees it plugged / unplugged).
    pub fn set_enabled(&mut self, on: bool) -> Result<(), String> {
        let file = Path::new(GADGET).join("UDC");
        let current = fs::read_to_string(&file).unwrap_or_default().trim().to_string();
        let want = if on { self.udc.as_str() } else { "" };
        if current != want {
            fs::write(&file, format!("{want}\n")).map_err(|e| match e.raw_os_error() {
                Some(libc::EBUSY) => "USB port is in use by something else".to_string(),
                _ => format!("USB: {e}"),
            })?;
        }
        if !on {
            self.connected.store(false, Ordering::Relaxed);
        }
        self.enabled = on;
        info!("USB transfer {}", if on { "on" } else { "off" });
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// A computer is connected and has set the device up.
    pub fn connected(&self) -> bool {
        self.enabled && self.connected.load(Ordering::Relaxed)
    }

    /// A file is being copied.
    pub fn busy(&self) -> bool {
        self.connected() && self.status.busy.load(Ordering::Relaxed)
    }
}

/// Set up by the root helper and usable by this user, so `Usb::start` works.
pub fn ready() -> bool {
    let writable = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_encoded_bytes())
            .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
    };
    udc_name().is_some() && writable(&Path::new(FFS_DIR).join("ep0")) && writable(&Path::new(GADGET).join("UDC"))
}

/// Whether this machine can be a USB device at all, or could after the Pi
/// overlay and a reboot.
pub fn supported() -> bool {
    udc_name().is_some() || is_pi()
}

fn udc_name() -> Option<String> {
    let mut names: Vec<String> = fs::read_dir(UDC_CLASS)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names.into_iter().next()
}

fn is_pi() -> bool {
    fs::read_to_string("/proc/device-tree/model").is_ok_and(|m| m.contains("Raspberry Pi"))
}

// --- endpoint 0: events and class requests -------------------------------------

fn control_loop(mut ep0: fs::File, udc: &str, connected: &AtomicBool, max_packet: &AtomicUsize, status: &mtp::Status) {
    let mut buf = [0u8; EVENT_SIZE * 4];
    loop {
        let n = match ep0.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                warn!("USB ep0: {e}");
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        for ev in buf[..n].chunks_exact(EVENT_SIZE) {
            match ev[8] {
                EV_ENABLE => {
                    let speed = fs::read_to_string(Path::new(UDC_CLASS).join(udc).join("current_speed"))
                        .unwrap_or_default();
                    let mp = match speed.trim() {
                        s if s.starts_with("super") => 1024,
                        "high-speed" => 512,
                        _ => 64,
                    };
                    max_packet.store(mp, Ordering::Relaxed);
                    connected.store(true, Ordering::Relaxed);
                    info!("USB: connected ({})", speed.trim());
                }
                EV_DISABLE | EV_UNBIND | EV_SUSPEND => {
                    if connected.swap(false, Ordering::Relaxed) {
                        info!("USB: disconnected");
                    }
                }
                EV_SETUP => setup_request(&mut ep0, ev, status),
                EV_BIND => {}
                _ => {}
            }
        }
    }
}

fn setup_request(ep0: &mut fs::File, ev: &[u8], status: &mtp::Status) {
    let request_type = ev[0];
    let request = ev[1];
    let length = u16::from_le_bytes([ev[6], ev[7]]) as usize;
    let device_to_host = request_type & 0x80 != 0;
    let class = request_type & 0x60 == 0x20;
    let result = match (class, request) {
        (true, REQ_CANCEL | REQ_DEVICE_RESET) if !device_to_host => {
            status.cancel.store(true, Ordering::Relaxed);
            let mut data = vec![0u8; length];
            ep0.read(&mut data).map(|_| ())
        }
        (true, REQ_GET_DEVICE_STATUS) if device_to_host => {
            // Length 4, OK.
            let reply = [4u8, 0, 0x01, 0x20];
            ep0.write(&reply[..reply.len().min(length)]).map(|_| ())
        }
        _ => {
            // Not ours: stall (a read on an IN request / a write on an OUT one).
            let _ = if device_to_host { ep0.read(&mut []) } else { ep0.write(&[]) };
            Ok(())
        }
    };
    if let Err(e) = result {
        info!("USB ep0 request {request:02x}: {e}");
    }
}

// --- bulk endpoints: MTP ------------------------------------------------------------

struct Endpoints {
    bulk_in: fs::File,
    bulk_out: fs::File,
    max_packet: Arc<AtomicUsize>,
}

impl Transport for Endpoints {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.bulk_out.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }

    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            // Zero-length packet.
            return self.bulk_in.write(data).map(|_| ());
        }
        self.bulk_in.write_all(data)
    }

    fn max_packet(&self) -> usize {
        self.max_packet.load(Ordering::Relaxed)
    }
}

fn data_loop(mtp: &mut Mtp, max_packet: &Arc<AtomicUsize>) {
    let open = |n: u8, write: bool| {
        fs::OpenOptions::new()
            .read(!write)
            .write(write)
            .custom_flags(libc::O_CLOEXEC)
            .open(Path::new(FFS_DIR).join(format!("ep{n}")))
    };
    let mut endpoints = None;
    loop {
        if endpoints.is_none() {
            match (open(1, true), open(2, false)) {
                (Ok(bulk_in), Ok(bulk_out)) => {
                    endpoints = Some(Endpoints { bulk_in, bulk_out, max_packet: max_packet.clone() })
                }
                (Err(e), _) | (_, Err(e)) => {
                    warn!("USB endpoints: {e}");
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
            }
        }
        if let Some(ep) = endpoints.as_mut() {
            // Blocks until a computer is connected; returns when it goes.
            if let Err(e) = mtp.serve(ep) {
                info!("MTP: {e}");
            }
            mtp.reset();
        }
        thread::sleep(Duration::from_millis(200));
    }
}

// --- descriptors -------------------------------------------------------------------

/// One interface (still image class 6/1/1, which is how computers spot MTP)
/// with bulk IN, bulk OUT and interrupt IN endpoints, at full and high speed.
fn descriptors() -> Vec<u8> {
    let speed = |bulk: u16| -> Vec<u8> {
        let mut d = vec![9, 4, 0, 0, 3, 6, 1, 1, 1]; // interface, 3 endpoints, string 1
        let ep = |addr: u8, attr: u8, size: u16, interval: u8| {
            let s = size.to_le_bytes();
            [7, 5, addr, attr, s[0], s[1], interval]
        };
        d.extend(ep(0x81, 2, bulk, 0)); // ep1: bulk IN
        d.extend(ep(0x02, 2, bulk, 0)); // ep2: bulk OUT
        d.extend(ep(0x83, 3, 28, 6)); // ep3: interrupt IN (events)
        d
    };
    let mut body = Vec::new();
    body.extend(4u32.to_le_bytes()); // full-speed descriptor count
    body.extend(4u32.to_le_bytes()); // high-speed
    body.extend(speed(64));
    body.extend(speed(512));
    let mut d = Vec::new();
    d.extend(DESCRIPTORS_MAGIC_V2.to_le_bytes());
    d.extend(((12 + body.len()) as u32).to_le_bytes());
    d.extend((HAS_FS_DESC | HAS_HS_DESC).to_le_bytes());
    d.extend(body);
    d
}

fn strings() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(1u32.to_le_bytes()); // strings per language
    body.extend(1u32.to_le_bytes()); // languages
    body.extend(0x0409u16.to_le_bytes()); // English (US)
    body.extend(b"MTP\0");
    let mut d = Vec::new();
    d.extend(STRINGS_MAGIC.to_le_bytes());
    d.extend(((8 + body.len()) as u32).to_le_bytes());
    d.extend(body);
    d
}

// --- setup (root) ----------------------------------------------------------------------

/// Root-owned script that sets the port up; run at every boot by the boot
/// step, and once by the root helper when USB transfer is first set up.
const SCRIPT: &str = "/usr/local/sbin/bman-usb-gadget";
/// Boot service name (systemd unit, OpenRC/runit/dinit service, rc.local block).
const SERVICE: &str = "bman-usb";
const MARKER: &str = "Written by bman (Settings > System > USB transfer)";

/// Set USB transfer up for this user, through sudo when not root. Blocking:
/// call from a thread.
pub fn setup(password: Option<&str>) -> Result<String, RootError> {
    let (uid, gid) = (euid(), unsafe { libc::getegid() });
    if uid == 0 {
        return setup_as_root(uid, gid).map_err(RootError::Failed);
    }
    run_helper(
        &["--usb".into(), "on".into(), "--uid".into(), uid.to_string(), "--gid".into(), gid.to_string()],
        password,
    )
}

/// Root side: `bman --usb on --uid U --gid G`, or `bman --usb off`
/// (the uninstaller: removes the boot step and the script).
pub fn cli_root(args: &[String]) -> Result<String, String> {
    // Run by hand (`sudo bman --usb on`): sudo says who ran it.
    let value = |flag: &str, env: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .or_else(|| std::env::var(env).ok())
            .and_then(|v| v.parse::<u32>().ok())
    };
    match (args.get(2).map(String::as_str), value("--uid", "SUDO_UID"), value("--gid", "SUDO_GID")) {
        (Some("on"), Some(uid), Some(gid)) => setup_as_root(uid, gid),
        (Some("off"), _, _) => remove_as_root(),
        (Some("on"), _, _) => Err("Run it with sudo as your user, or pass --uid UID --gid GID".into()),
        _ => Err("Usage: --usb on [--uid UID --gid GID] | --usb off".into()),
    }
}

/// The boot script for this user. Only fixed text plus two numbers.
fn script(uid: u32, gid: u32) -> String {
    format!(
        r#"#!/bin/sh
# {MARKER}
# Runs as root at boot: makes the USB gadget bman's "USB transfer" uses
# (configfs, FunctionFS at {FFS_DIR}) and hands it to uid {uid}. bman binds
# it to the port itself. Root-owned on purpose; bman doesn't run as root.
uid={uid}
gid={gid}
g={GADGET}
ffs={FFS_DIR}
[ -d {CONFIGFS} ] || modprobe configfs 2>/dev/null
grep -qs ' {CONFIGFS} ' /proc/mounts || mount -t configfs configfs {CONFIGFS} || exit 1
[ -d {CONFIGFS}/usb_gadget ] || modprobe libcomposite || exit 1
modprobe usb_f_fs 2>/dev/null
if [ ! -d "$g" ]; then
    mkdir "$g" || exit 1
    echo 0x1d6b > "$g/idVendor"
    echo 0x0104 > "$g/idProduct"
    echo 0x0200 > "$g/bcdUSB"
    mkdir -p "$g/strings/0x409" "$g/configs/c.1/strings/0x409" "$g/functions/ffs.{FFS_NAME}"
    echo bman > "$g/strings/0x409/manufacturer"
    echo bman > "$g/strings/0x409/product"
    serial=$(cat /etc/machine-id /var/lib/dbus/machine-id 2>/dev/null | head -n 1 | cut -c 1-16)
    echo "${{serial:-bman}}" > "$g/strings/0x409/serialnumber"
    echo MTP > "$g/configs/c.1/strings/0x409/configuration"
    echo 250 > "$g/configs/c.1/MaxPower"
    ln -s "$g/functions/ffs.{FFS_NAME}" "$g/configs/c.1/" || exit 1
fi
mkdir -p "$ffs"
grep -qs " $ffs " /proc/mounts || mount -t functionfs -o "uid=$uid,gid=$gid" {FFS_NAME} "$ffs" || exit 1
chown "$uid:$gid" "$ffs" "$g/UDC"
"#
    )
}

fn setup_as_root(uid: u32, gid: u32) -> Result<String, String> {
    let restart = udc_name().is_none();
    if restart {
        if !is_pi() {
            return Err("No USB device port on this computer".into());
        }
        add_overlay()?;
    }
    let path = Path::new(SCRIPT);
    crate::autostart::write_marked(path, &script(uid, gid), 0o755, MARKER)?;
    // root:root, whoever made /usr/local/sbin.
    let _ = std::os::unix::fs::chown(path, Some(0), Some(0));
    match crate::autostart::detect() {
        Some(init) => crate::autostart::install_boot_step(init, SERVICE, SCRIPT, MARKER)?,
        None => warn!("Unknown init system: USB transfer needs setting up after every boot"),
    }
    // And now, so it works without a reboot (if the port is there).
    let out = std::process::Command::new(SCRIPT).output().map_err(|e| format!("{SCRIPT}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("USB setup failed: {}", err.lines().last().unwrap_or("?")));
    }
    if restart {
        return Ok("USB transfer: restart the Pi to finish".into());
    }
    info!("USB gadget ready");
    Ok("USB transfer set up".into())
}

/// Take down the gadget and remove the boot step and script.
fn remove_as_root() -> Result<String, String> {
    let g = Path::new(GADGET);
    if g.is_dir() {
        let _ = fs::write(g.join("UDC"), "\n");
    }
    let ffs = std::ffi::CString::new(FFS_DIR).unwrap_or_default();
    unsafe { libc::umount2(ffs.as_ptr(), libc::MNT_DETACH) };
    let _ = fs::remove_dir(FFS_DIR);
    if g.is_dir() {
        // configfs: links and directories, deepest first.
        let _ = fs::remove_file(g.join(format!("configs/c.1/ffs.{FFS_NAME}")));
        for d in [
            "configs/c.1/strings/0x409",
            "configs/c.1",
            &format!("functions/ffs.{FFS_NAME}"),
            "strings/0x409",
            "",
        ] {
            let _ = fs::remove_dir(g.join(d));
        }
    }
    if let Some(init) = crate::autostart::detect() {
        crate::autostart::remove_boot_step(init, SERVICE, MARKER)?;
    }
    if fs::read_to_string(SCRIPT).is_ok_and(|t| t.contains(MARKER)) {
        fs::remove_file(SCRIPT).map_err(|e| format!("{SCRIPT}: {e}"))?;
    }
    Ok("USB transfer removed".into())
}

/// Add the dwc2 overlay to the Pi's config.txt (once).
fn add_overlay() -> Result<(), String> {
    let path = ["/boot/firmware/config.txt", "/boot/config.txt"]
        .into_iter()
        .find(|p| Path::new(p).is_file())
        .ok_or("Can't find the Pi's config.txt")?;
    let text = fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    if !has_overlay(&text) {
        let mut f = fs::OpenOptions::new().append(true).open(path).map_err(|e| format!("{path}: {e}"))?;
        let sep = if text.ends_with('\n') { "" } else { "\n" };
        writeln!(f, "{sep}\n[all]\n# bman: USB transfer (the data port acts as a device)\n{OVERLAY}")
            .map_err(|e| format!("{path}: {e}"))?;
        info!("Added {OVERLAY} to {path}");
    }
    Ok(())
}

/// A `dtoverlay=dwc2` line that applies to every Pi: before any `[filter]`
/// section or under `[all]` (the stock file has one under `[cm5]`, in host
/// mode, which doesn't count), and not in host mode.
fn has_overlay(config: &str) -> bool {
    let mut applies = true;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            applies = line.eq_ignore_ascii_case("[all]");
        } else if applies && line.starts_with("dtoverlay=dwc2") && !line.contains("dr_mode=host") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    #[test]
    fn overlay_only_counts_where_it_applies() {
        let stock = "arm_64bit=1\n[cm4]\notg_mode=1\n[cm5]\ndtoverlay=dwc2,dr_mode=host\n[all]\ndtparam=i2s=on\n";
        assert!(!super::has_overlay(stock));
        assert!(super::has_overlay(&format!("{stock}{}\n", super::OVERLAY)));
        assert!(super::has_overlay("dtoverlay=dwc2\n[pi4]\nfoo=1\n"));
        assert!(!super::has_overlay("[all]\ndtoverlay=dwc2,dr_mode=host\n"));
    }

    /// The boot script is valid sh (`sh -n`) and has the user's ids.
    #[test]
    fn boot_script() {
        let text = super::script(1000, 1001);
        assert!(text.contains("uid=1000\ngid=1001") && text.contains("${serial:-bman}"));
        let path = std::env::temp_dir().join(format!("bman-usb-script-{}", std::process::id()));
        std::fs::write(&path, &text).unwrap();
        let ok = std::process::Command::new("sh").arg("-n").arg(&path).status().unwrap().success();
        std::fs::remove_file(&path).unwrap();
        assert!(ok);
    }
}

#[cfg(test)]
mod live {
    /// Needs a UDC and the gadget set up first (see DOCS.md, testing on a PC):
    ///   sudo modprobe dummy_hcd && sudo target/release/bman --usb on --uid $(id -u) --gid $(id -g)
    /// (that also installs the bman-usb boot service on the PC; `--usb off`
    /// removes it), then `cargo test --release usb_live -- --ignored --nocapture` serves
    /// BMAN_USB_DIR/{music,videos} for BMAN_USB_SECS seconds (default 60).
    #[test]
    #[ignore]
    fn usb_live() {
        let dir = std::path::PathBuf::from(std::env::var("BMAN_USB_DIR").unwrap_or("/tmp/bman-usb".into()));
        let secs: u64 = std::env::var("BMAN_USB_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
        let status = crate::mtp::Status::default();
        let usb = super::Usb::start(vec![("Music", dir.join("music")), ("Videos", dir.join("videos"))], status.clone())
            .expect("start");
        println!("serving {} for {secs} s", dir.display());
        for _ in 0..secs * 10 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if status.changed.swap(false, std::sync::atomic::Ordering::Relaxed) {
                println!("changed (connected: {})", usb.connected());
            }
        }
    }
}
