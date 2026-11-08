//! Restart / shut down the device. Without root if possible: ask the login
//! manager over D-Bus (logind, from systemd or elogind, then ConsoleKit2),
//! which lets a user in an active session do it. A background service has
//! no session, so then it goes through sudo (root.rs), which on Raspberry
//! Pi OS needs no password, and runs `reboot` / `poweroff` as root.

use zbus::blocking::{Connection, Proxy};

use crate::root::{RootError, euid, run_helper};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Reboot,
    PowerOff,
}

impl PowerAction {
    fn arg(self) -> &'static str {
        match self {
            PowerAction::Reboot => "reboot",
            PowerAction::PowerOff => "poweroff",
        }
    }

    pub fn doing(self) -> &'static str {
        match self {
            PowerAction::Reboot => "Restarting...",
            PowerAction::PowerOff => "Shutting down...",
        }
    }
}

/// Restart or shut down. Blocking (D-Bus, maybe sudo): call from a thread.
/// `password` is the sudo password, after a first try said it needs one.
pub fn request(action: PowerAction, password: Option<&str>) -> Result<String, RootError> {
    if password.is_none() {
        match login_manager(action) {
            Ok(()) => return Ok(action.doing().into()),
            Err(e) => info!("Login manager can't {}: {e}, trying sudo", action.arg()),
        }
    }
    if euid() == 0 {
        return run_command(action).map_err(RootError::Failed);
    }
    run_helper(&["--power".into(), action.arg().into()], password)
}

/// logind first, then ConsoleKit2. Only goes ahead when it answers "yes"
/// ("challenge" would need a password prompt we can't show).
fn login_manager(action: PowerAction) -> Result<(), String> {
    let conn = Connection::system().map_err(|_| "no system D-Bus".to_string())?;
    let (can, method) = match action {
        PowerAction::Reboot => ("CanReboot", "Reboot"),
        PowerAction::PowerOff => ("CanPowerOff", "PowerOff"),
    };
    let managers = [
        ("org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager"),
        (
            "org.freedesktop.ConsoleKit",
            "/org/freedesktop/ConsoleKit/Manager",
            "org.freedesktop.ConsoleKit.Manager",
        ),
    ];

    let mut last = "no login manager".to_string();
    for (dest, path, iface) in managers {
        let Ok(proxy) = Proxy::new(&conn, dest, path, iface) else { continue };
        match proxy.call::<_, _, String>(can, &()) {
            Ok(answer) if answer == "yes" => {
                return proxy
                    .call::<_, _, ()>(method, &(false,))
                    .map_err(|e| e.to_string());
            }
            Ok(answer) => last = format!("{dest} says {answer}"),
            Err(_) => {} // not running
        }
    }
    Err(last)
}

/// As root: run the system's own `reboot` / `poweroff`, which every init
/// provides, so it shuts down cleanly.
fn run_command(action: PowerAction) -> Result<String, String> {
    let status = std::process::Command::new(action.arg())
        .status()
        .map_err(|e| format!("Can't run {}: {e}", action.arg()))?;
    if status.success() {
        Ok(action.doing().into())
    } else {
        Err(format!("{} failed", action.arg()))
    }
}

/// Root side: `bman --power reboot|poweroff`.
pub fn cli_root(args: &[String]) -> Result<String, String> {
    match args.get(2).map(String::as_str) {
        Some("reboot") => run_command(PowerAction::Reboot),
        Some("poweroff") => run_command(PowerAction::PowerOff),
        _ => Err("Usage: --power reboot|poweroff".into()),
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Asks (never reboots): `cargo test power_can -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn power_can() {
        let conn = Connection::system().expect("system bus");
        for (dest, path, iface) in [
            ("org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager"),
            ("org.freedesktop.ConsoleKit", "/org/freedesktop/ConsoleKit/Manager", "org.freedesktop.ConsoleKit.Manager"),
        ] {
            let proxy = Proxy::new(&conn, dest, path, iface).unwrap();
            for m in ["CanReboot", "CanPowerOff"] {
                println!("{dest} {m}: {:?}", proxy.call::<_, _, String>(m, &()).map_err(|e| e.to_string()));
            }
        }
    }
}
