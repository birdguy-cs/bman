//! Start bman at boot. There's no init-agnostic way to do this, so this file
//! is the one place that knows about init systems. It reads PID 1's name
//! (falling back to the marks each init leaves in /run) and writes a boot
//! service that runs this binary, from the current folder, as the user who
//! started it (`SUDO_USER` when run with sudo):
//!
//! | Init | Used by (default) | What's written |
//! |---|---|---|
//! | systemd | Debian, Raspberry Pi OS, Ubuntu, Pop!_OS, Mint, Fedora, Arch, Manjaro, SteamOS, openSUSE | unit + `multi-user.target.wants` link |
//! | OpenRC | Gentoo, Alpine, Artix-OpenRC, Devuan (option) | `/etc/init.d` script + `runlevels/default` link |
//! | runit | Void, Artix-runit, Devuan (option) | `run` script + service link (3 layouts) |
//! | dinit | Chimera, Artix-dinit | service file + `boot.d` link |
//! | sysvinit / BSD-style | Devuan, MX Linux, antiX, Slackware | a marked block in `rc.local` |
//!
//! Only files are written; no init commands are run, so the change takes
//! effect at the next boot. Every file written carries MARKER, and only
//! files (or rc.local blocks) with it are removed.
//!
//! Writing into /etc needs root. When bman isn't root, `set` re-runs this
//! binary through sudo (see root.rs) as `bman --autostart on|off --user NAME
//! --uid N --dir DIR`, handled by `cli_root`.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use crate::root::{RootError, euid, exe_path};

const MARKER: &str = "Written by bman (Settings > System > Start on boot)";
/// rc.local block delimiters.
const BLOCK_START: &str = "# >>> bman";
const BLOCK_END: &str = "# <<< bman";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Init {
    Systemd,
    OpenRc,
    /// Void: services in /etc/sv, enabled in /var/service.
    RunitVoid,
    /// Artix: services in /etc/runit/sv, enabled in /etc/runit/runsvdir/default.
    RunitArtix,
    /// Devuan / Debian runit: services in /etc/sv, enabled in /etc/service.
    RunitDebian,
    Dinit,
    /// sysvinit (Devuan, MX Linux, antiX): /etc/rc.local.
    RcLocal,
    /// Slackware: /etc/rc.d/rc.local.
    RcLocalSlackware,
}

impl Init {
    pub fn name(self) -> &'static str {
        match self {
            Init::Systemd => "systemd",
            Init::OpenRc => "OpenRC",
            Init::RunitVoid | Init::RunitArtix | Init::RunitDebian => "runit",
            Init::Dinit => "dinit",
            Init::RcLocal | Init::RcLocalSlackware => "rc.local",
        }
    }

    /// (service file, enable link). For rc.local both are the rc.local file.
    fn paths(self) -> (PathBuf, PathBuf) {
        let p = PathBuf::from;
        match self {
            Init::Systemd => (
                p("/etc/systemd/system/bman.service"),
                p("/etc/systemd/system/multi-user.target.wants/bman.service"),
            ),
            Init::OpenRc => (p("/etc/init.d/bman"), p("/etc/runlevels/default/bman")),
            Init::RunitVoid => (p("/etc/sv/bman"), p("/var/service/bman")),
            Init::RunitArtix => (p("/etc/runit/sv/bman"), p("/etc/runit/runsvdir/default/bman")),
            Init::RunitDebian => (p("/etc/sv/bman"), p("/etc/service/bman")),
            Init::Dinit => (p("/etc/dinit.d/bman"), p("/etc/dinit.d/boot.d/bman")),
            Init::RcLocal => (p("/etc/rc.local"), p("/etc/rc.local")),
            Init::RcLocalSlackware => (p("/etc/rc.d/rc.local"), p("/etc/rc.d/rc.local")),
        }
    }
}

/// Which init system is running. PID 1's name is the most reliable sign;
/// "init" is ambiguous (sysvinit or BusyBox, maybe running OpenRC), so then
/// the marks in /run and /etc decide.
pub fn detect() -> Option<Init> {
    let pid1 = fs::read_to_string("/proc/1/comm").unwrap_or_default();
    detect_from(pid1.trim(), &|p| Path::new(p).exists())
}

fn detect_from(pid1: &str, exists: &dyn Fn(&str) -> bool) -> Option<Init> {
    let runit = || {
        // Void also has /etc/runit/runsvdir, so its layout is checked first.
        if exists("/var/service") && exists("/etc/sv") {
            Some(Init::RunitVoid)
        } else if exists("/etc/runit/runsvdir/default") {
            Some(Init::RunitArtix)
        } else if exists("/etc/service") && exists("/etc/sv") {
            Some(Init::RunitDebian)
        } else {
            None
        }
    };

    match pid1 {
        "systemd" => return Some(Init::Systemd),
        "runit" => return runit(),
        "dinit" => return Some(Init::Dinit),
        "openrc-init" => return Some(Init::OpenRc),
        _ => {}
    }

    if exists("/run/systemd/system") {
        Some(Init::Systemd)
    } else if exists("/run/openrc") {
        Some(Init::OpenRc)
    } else if exists("/run/runit") {
        runit()
    } else if exists("/run/dinitctl") {
        Some(Init::Dinit)
    } else if exists("/etc/rc.d/rc.local") {
        Some(Init::RcLocalSlackware)
    } else if exists("/etc/inittab") || exists("/etc/rc.local") || exists("/etc/init.d/rc.local") {
        Some(Init::RcLocal)
    } else {
        None
    }
}

/// Is the bman boot service enabled?
pub fn is_enabled(init: Init) -> bool {
    let (service, link) = init.paths();
    match init {
        Init::RcLocal | Init::RcLocalSlackware => fs::read_to_string(service)
            .map(|t| t.contains(MARKER))
            .unwrap_or(false),
        _ => fs::symlink_metadata(link).is_ok(),
    }
}

/// Who the service runs as.
struct User {
    name: String,
    uid: u32,
}

/// Login name for a uid, from /etc/passwd (falling back to $USER).
fn user_name(uid: u32) -> String {
    fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.get(2).and_then(|u| u.parse::<u32>().ok()) == Some(uid))
        .and_then(|f| f.first().map(|n| n.to_string()))
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "root".into())
}

/// Already root: run as the sudo user if started with sudo, else as root.
fn root_target_user() -> User {
    let sudo = std::env::var("SUDO_USER").ok().filter(|n| !n.is_empty());
    let uid = std::env::var("SUDO_UID").ok().and_then(|u| u.parse().ok());
    match (sudo, uid) {
        (Some(name), Some(uid)) => User { name, uid },
        _ => User { name: "root".into(), uid: 0 },
    }
}

/// Turn start on boot on or off, getting root through sudo when needed.
/// Blocking (sudo can take a couple of seconds): call it from a thread.
pub fn set(init: Init, on: bool, password: Option<&str>) -> Result<String, RootError> {
    let dir = std::env::current_dir().map_err(|e| RootError::Failed(e.to_string()))?;
    let dir = dir.to_string_lossy().to_string();

    if euid() == 0 {
        let user = root_target_user();
        return if on {
            enable(init, &user, &dir)
        } else {
            disable(init).map(|()| "Won't start on boot".into())
        }
        .map_err(RootError::Failed);
    }

    let uid = euid();
    let helper_args = [
        "--autostart".to_string(),
        if on { "on" } else { "off" }.to_string(),
        "--user".into(),
        user_name(uid),
        "--uid".into(),
        uid.to_string(),
        "--dir".into(),
        dir,
    ];
    crate::root::run_helper(&helper_args, password)
}

/// Root side of `set`: `bman --autostart on|off --user NAME --uid N --dir DIR`
/// (called by `root::cli` once it has checked we're root).
pub fn cli_root(args: &[String]) -> Result<String, String> {
    let value = |key: &str| {
        args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned()
    };
    let init = detect().ok_or("Unknown init system")?;
    if args.get(2).map(String::as_str) == Some("on") {
        let user = User {
            name: value("--user").ok_or("Missing --user")?,
            uid: value("--uid").and_then(|u| u.parse().ok()).ok_or("Missing --uid")?,
        };
        enable(init, &user, &value("--dir").ok_or("Missing --dir")?)
    } else {
        disable(init).map(|()| "Won't start on boot".to_string())
    }
}

/// Names of the user's primary group and every group listing them, read
/// from /etc/passwd and /etc/group. runit's chpst doesn't look these up
/// itself, and bman needs e.g. gpio, spi and audio.
fn groups_of(user: &str) -> Vec<String> {
    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    let group = fs::read_to_string("/etc/group").unwrap_or_default();
    let primary_gid = passwd
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.first() == Some(&user))
        .and_then(|f| f.get(3).map(|g| g.to_string()));

    let mut primary = None;
    let mut rest = Vec::new();
    for line in group.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 4 {
            continue;
        }
        if Some(f[2]) == primary_gid.as_deref() {
            primary = Some(f[0].to_string());
        } else if f[3].split(',').any(|m| m == user) {
            rest.push(f[0].to_string());
        }
    }
    primary.into_iter().chain(rest).collect()
}

/// Quote for a POSIX shell script.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn friendly(e: std::io::Error) -> String {
    match e.kind() {
        ErrorKind::PermissionDenied => "Needs root (run with sudo)".into(),
        ErrorKind::ReadOnlyFilesystem => "System files are read-only".into(),
        _ => e.to_string(),
    }
}

/// rc.local with bman's block added: before the final `exit 0` if there is
/// one, else at the end. A new file gets a shebang and `exit 0`.
fn add_block(text: &str, block: &str) -> String {
    let text = remove_block(text);
    if text.trim().is_empty() {
        return format!("#!/bin/sh\n\n{block}\nexit 0\n");
    }
    let mut lines: Vec<&str> = text.lines().collect();
    let at = lines.iter().rposition(|l| l.trim() == "exit 0").unwrap_or(lines.len());
    lines.insert(at, block.trim_end());
    lines.join("\n") + "\n"
}

/// rc.local without bman's block.
fn remove_block(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with(BLOCK_START) {
            inside = true;
        } else if inside && line.starts_with(BLOCK_END) {
            inside = false;
        } else if !inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Write and enable the boot service. Returns a short message for a toast.
fn enable(init: Init, user: &User, dir: &str) -> Result<String, String> {
    let exe = exe_path()?;
    let exe = exe.to_string_lossy();
    let (service, link) = init.paths();

    match init {
        Init::Systemd => {
            let unit = format!(
                "# {MARKER}\n\
                 [Unit]\n\
                 Description=bman music player\n\
                 After=network.target sound.target bluetooth.target\n\n\
                 [Service]\n\
                 User={name}\n\
                 WorkingDirectory={dir}\n\
                 ExecStart=\"{exe}\"\n\
                 Environment=XDG_RUNTIME_DIR=/run/user/{uid}\n\
                 Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus\n\
                 Restart=on-failure\n\
                 RestartSec=3\n\n\
                 [Install]\n\
                 WantedBy=multi-user.target\n",
                name = user.name,
                uid = user.uid,
            );
            write_file(&service, &unit, 0o644)?;
            // Lingering starts the user's own services (PipeWire) at boot, so
            // Bluetooth audio works without logging in.
            if user.uid != 0 {
                let linger = Path::new("/var/lib/systemd/linger").join(&user.name);
                if fs::create_dir_all("/var/lib/systemd/linger").is_ok() && !linger.exists() {
                    let _ = fs::write(linger, "");
                }
            }
            make_link(&service, &link)?;
        }
        Init::OpenRc => {
            let script = format!(
                "#!/sbin/openrc-run\n\
                 # {MARKER}\n\
                 description=\"bman music player\"\n\
                 command={exe}\n\
                 command_user={name}\n\
                 directory={dir}\n\
                 command_background=true\n\
                 pidfile=/run/bman.pid\n\n\
                 depend() {{\n\
                 \tneed localmount\n\
                 \tafter bootmisc net bluetooth alsasound\n\
                 }}\n",
                exe = sh_quote(&exe),
                name = sh_quote(&user.name),
                dir = sh_quote(&dir),
            );
            write_file(&service, &script, 0o755)?;
            make_link(&service, &link)?;
        }
        Init::RunitVoid | Init::RunitArtix | Init::RunitDebian => {
            let groups = groups_of(&user.name);
            let who = std::iter::once(user.name.clone()).chain(groups).collect::<Vec<_>>().join(":");
            let run = format!(
                "#!/bin/sh\n\
                 # {MARKER}\n\
                 export XDG_RUNTIME_DIR=/run/user/{uid}\n\
                 cd {dir} || exit 1\n\
                 exec chpst -u {who} {exe} 2>&1\n",
                uid = user.uid,
                dir = sh_quote(&dir),
                who = sh_quote(&who),
                exe = sh_quote(&exe),
            );
            fs::create_dir_all(&service).map_err(friendly)?;
            write_file(&service.join("run"), &run, 0o755)?;
            make_link(&service, &link)?;
        }
        Init::RcLocal | Init::RcLocalSlackware => {
            let inner = format!("cd {} && exec {}", sh_quote(&dir), sh_quote(&exe));
            let block = format!(
                "{BLOCK_START} ({MARKER})\n\
                 su -s /bin/sh {name} -c {cmd} >/dev/null 2>&1 &\n\
                 {BLOCK_END}\n",
                name = sh_quote(&user.name),
                cmd = sh_quote(&inner),
            );
            let text = add_block(&fs::read_to_string(&service).unwrap_or_default(), &block);
            if let Some(parent) = service.parent() {
                fs::create_dir_all(parent).map_err(friendly)?;
            }
            fs::write(&service, text).map_err(friendly)?;
            fs::set_permissions(&service, fs::Permissions::from_mode(0o755)).map_err(friendly)?;
        }
        Init::Dinit => {
            let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
            let conf = format!(
                "# {MARKER}\n\
                 type = process\n\
                 command = {exe}\n\
                 working-dir = {dir}\n\
                 run-as = {name}\n\
                 restart = false\n",
                exe = quote(&exe),
                dir = quote(&dir),
                name = user.name,
            );
            write_file(&service, &conf, 0o644)?;
            make_link(&service, &link)?;
        }
    }

    info!("Start on boot enabled ({}): {}", init.name(), service.display());
    Ok(format!("Starts on boot ({})", init.name()))
}

/// Remove the boot service (only files this module wrote).
fn disable(init: Init) -> Result<(), String> {
    let (service, link) = init.paths();
    if matches!(init, Init::RcLocal | Init::RcLocalSlackware) {
        let text = fs::read_to_string(&service).map_err(friendly)?;
        fs::write(&service, remove_block(&text)).map_err(friendly)?;
        info!("Start on boot disabled ({})", init.name());
        return Ok(());
    }
    let file = if service.is_dir() { service.join("run") } else { service.clone() };
    let ours = fs::read_to_string(&file).map(|t| t.contains(MARKER)).unwrap_or(false);
    if !ours && file.exists() {
        return Err("Not bman's service, left it".into());
    }

    match fs::remove_file(&link) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(friendly(e)),
    }
    if service.is_dir() {
        fs::remove_dir_all(&service).map_err(friendly)?;
    } else if service.exists() {
        fs::remove_file(&service).map_err(friendly)?;
    }
    info!("Start on boot disabled ({})", init.name());
    Ok(())
}

fn write_file(path: &Path, contents: &str, mode: u32) -> Result<(), String> {
    if let Ok(existing) = fs::read_to_string(path) {
        if !existing.contains(MARKER) {
            return Err("A bman service exists already".into());
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(friendly)?;
    }
    fs::write(path, contents).map_err(friendly)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(friendly)
}

fn make_link(target: &Path, link: &Path) -> Result<(), String> {
    if fs::symlink_metadata(link).is_ok() {
        return Ok(());
    }
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).map_err(friendly)?;
    }
    symlink(target, link).map_err(friendly)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection() {
        let has = |paths: &'static [&'static str]| move |p: &str| paths.contains(&p);
        assert_eq!(detect_from("systemd", &has(&[])), Some(Init::Systemd));
        let void = has(&["/var/service", "/etc/sv", "/etc/runit/runsvdir/default", "/run/runit"]);
        assert_eq!(detect_from("runit", &void), Some(Init::RunitVoid));
        let artix = has(&["/etc/runit/runsvdir/default", "/run/runit"]);
        assert_eq!(detect_from("runit", &artix), Some(Init::RunitArtix));
        let devuan_runit = has(&["/etc/service", "/etc/sv", "/run/runit"]);
        assert_eq!(detect_from("runit", &devuan_runit), Some(Init::RunitDebian));
        // Gentoo / Alpine: sysvinit or BusyBox as PID 1 running OpenRC.
        assert_eq!(detect_from("init", &has(&["/run/openrc", "/etc/inittab"])), Some(Init::OpenRc));
        assert_eq!(detect_from("dinit", &has(&[])), Some(Init::Dinit));
        // Devuan / MX Linux / antiX on sysvinit.
        assert_eq!(detect_from("init", &has(&["/etc/inittab", "/etc/rc.local"])), Some(Init::RcLocal));
        assert_eq!(
            detect_from("init", &has(&["/etc/inittab", "/etc/rc.d/rc.local"])),
            Some(Init::RcLocalSlackware)
        );
        assert_eq!(detect_from("s6-svscan", &has(&[])), None);
    }

    #[test]
    fn rc_local_block() {
        let block = "# >>> bman (x)\nsu ...\n# <<< bman\n";
        let original = "#!/bin/sh -e\necho hi\nexit 0\n";
        let added = add_block(original, block);
        assert_eq!(added, "#!/bin/sh -e\necho hi\n# >>> bman (x)\nsu ...\n# <<< bman\nexit 0\n");
        assert_eq!(add_block(&added, block), added); // no duplicates
        assert_eq!(remove_block(&added), original);
        assert!(add_block("", block).starts_with("#!/bin/sh"));
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }
}

#[cfg(test)]
mod live {
    /// Runs `set` against whatever `sudo` is first in PATH, so point PATH at
    /// a fake one: `PATH=/tmp/fake:$PATH cargo test sudo_flow -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn sudo_flow() {
        let init = super::detect().expect("init");
        let pw = std::env::var("FAKE_PW").ok();
        match super::set(init, true, pw.as_deref()) {
            Ok(m) => println!("result: ok {m}"),
            Err(crate::root::RootError::NeedsPassword) => println!("result: needs password"),
            Err(super::RootError::WrongPassword) => println!("result: wrong password"),
            Err(super::RootError::Failed(e)) => println!("result: failed {e}"),
        }
    }
}
