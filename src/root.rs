//! Running things as root. bman itself never runs as root (unless you start
//! it that way): for the few jobs that need it (start on boot, restart /
//! shut down when the login manager says no), it re-runs its own binary
//! through `sudo` with a hidden flag (`--autostart ...` or `--power ...`),
//! does the job in that short-lived child, and reads back "OK <message>" or
//! "ERR <message>".
//!
//! `sudo -n` first, which works with passwordless sudo (the Raspberry Pi OS
//! default); if sudo wants a password the caller asks for it on the keyboard
//! and passes it to `sudo -S` on stdin (never in arguments, never saved).
//! Without sudo, passwordless `doas` is tried. This and power.rs are the
//! only places bman runs other programs.

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Why a root job didn't work.
pub enum RootError {
    /// sudo wants a password: ask for it and try again with it.
    NeedsPassword,
    WrongPassword,
    Failed(String),
}

pub fn euid() -> u32 {
    fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(u32::MAX)
}

/// The running binary (after an update Linux appends " (deleted)").
pub fn exe_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let text = exe.to_string_lossy();
    Ok(text.strip_suffix(" (deleted)").map(PathBuf::from).unwrap_or(exe))
}

/// Run `bman <helper_args>` as root through sudo. Blocking: call from a thread.
pub fn run_helper(helper_args: &[String], password: Option<&str>) -> Result<String, RootError> {
    let exe = exe_path().map_err(RootError::Failed)?;

    let run = |program: &str, flags: &[&str], stdin: Option<&str>| {
        let mut child = Command::new(program)
            .args(flags)
            .arg("--")
            .arg(&exe)
            .args(helper_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut pipe) = child.stdin.take() {
            if let Some(text) = stdin {
                let _ = pipe.write_all(format!("{text}\n").as_bytes());
            }
            // Dropped here, so a wrong password reads EOF instead of hanging.
        }
        child.wait_with_output()
    };

    let output = match password {
        None => run("sudo", &["-n"], None),
        Some(pw) => run("sudo", &["-S", "-p", ""], Some(pw)),
    };
    let output = match output {
        Ok(o) => o,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // No sudo: try doas, which can only be used here without a password.
            match run("doas", &["-n"], None) {
                Ok(o) if o.status.success() => o,
                Ok(_) => return Err(RootError::Failed("doas needs nopass; or run as root".into())),
                Err(_) => return Err(RootError::Failed("Needs root: no sudo found".into())),
            }
        }
        Err(e) => return Err(RootError::Failed(e.to_string())),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    let reply = stdout.lines().find_map(|l| {
        l.strip_prefix("OK ")
            .or(l.strip_prefix("ERR "))
            .map(|m| (l.starts_with("OK "), m))
    });

    match reply {
        Some((true, msg)) => Ok(msg.to_string()),
        Some((false, msg)) => Err(RootError::Failed(msg.to_string())),
        None if stderr.contains("incorrect password") || stderr.contains("try again") => {
            Err(RootError::WrongPassword)
        }
        None if stderr.contains("password is required")
            || stderr.contains("terminal is required")
            || stderr.contains("no tty") =>
        {
            if password.is_some() {
                Err(RootError::WrongPassword)
            } else {
                Err(RootError::NeedsPassword)
            }
        }
        None if stderr.contains("sudoers") || stderr.contains("not allowed") => {
            Err(RootError::Failed("Not allowed to use sudo".into()))
        }
        None => {
            let line = stderr.lines().next().unwrap_or("sudo failed").to_string();
            warn!("sudo: {}", String::from_utf8_lossy(&output.stderr).trim());
            Err(RootError::Failed(line))
        }
    }
}

/// The root side: handles `bman --autostart ...` / `bman --power ...`.
/// Returns the exit code, or None when started normally.
pub fn cli() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    let job = args.get(1).map(String::as_str);
    if !matches!(job, Some("--autostart" | "--power")) {
        return None;
    }
    let result = if euid() != 0 {
        Err("Needs root".to_string())
    } else if job == Some("--autostart") {
        crate::autostart::cli_root(&args)
    } else {
        crate::power::cli_root(&args)
    };
    match result {
        Ok(msg) => {
            println!("OK {msg}");
            Some(0)
        }
        Err(e) => {
            println!("ERR {e}");
            Some(1)
        }
    }
}
