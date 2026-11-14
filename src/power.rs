//! Restart / shut down the device. Without root if possible: ask the login
//! manager over D-Bus (logind, from systemd or elogind, then ConsoleKit2).
//! By default that only lets someone at the device's own screen do it, not
//! an SSH session or a boot service, so then it goes through sudo (root.rs)
//! and runs `reboot` / `poweroff` as root.
//!
//! That sudo run also installs a polkit rule (POLKIT_RULE) allowing just
//! this user to restart / shut down, so from then on the login manager says
//! yes and no sudo or password is needed again. Start on boot installs it
//! too. polkit has to be installed for the rule to matter (it usually is;
//! on a minimal Debian / Raspberry Pi OS it's the `polkitd` package).

use std::fs;
use std::path::Path;

use zbus::blocking::{Connection, Proxy};

use crate::root::{RootError, euid, run_helper, user_name};

const POLKIT_DIR: &str = "/etc/polkit-1/rules.d";
const POLKIT_RULE: &str = "/etc/polkit-1/rules.d/50-bman-power.rules";
const MARKER: &str = "Written by bman";

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
    let user = user_name(euid());
    run_helper(&["--power".into(), action.arg().into(), "--user".into(), user], password)
}

/// As root: let `user` restart and shut down through the login manager
/// without a password (logind / elogind and ConsoleKit2 actions). Only
/// writes when polkit is installed; never touches a file bman didn't write.
pub fn install_polkit_rule(user: &str) -> Result<(), String> {
    if !Path::new(POLKIT_DIR).is_dir() {
        return Err("polkit isn't installed (no /etc/polkit-1/rules.d)".into());
    }
    // A user name in the rule has to be a plain name.
    if user.is_empty() || !user.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return Err(format!("Unusual user name '{user}', not writing a rule"));
    }
    if let Ok(existing) = fs::read_to_string(POLKIT_RULE)
        && !existing.contains(MARKER) {
            return Err(format!("{POLKIT_RULE} exists and isn't bman's"));
        }
    fs::write(POLKIT_RULE, rule_text(user)).map_err(|e| e.to_string())?;
    info!("Installed {POLKIT_RULE}: {user} can now restart / shut down without sudo");
    Ok(())
}

/// The polkit rule (JavaScript) allowing `user` to restart / shut down.
fn rule_text(user: &str) -> String {
    format!(
        "// {MARKER}: lets {user} restart / shut down without a password\n\
         // (Settings > System). Delete this file to undo.\n\
         polkit.addRule(function(action, subject) {{\n\
         \x20   var power = [\n\
         \x20       \"org.freedesktop.login1.reboot\",\n\
         \x20       \"org.freedesktop.login1.reboot-multiple-sessions\",\n\
         \x20       \"org.freedesktop.login1.power-off\",\n\
         \x20       \"org.freedesktop.login1.power-off-multiple-sessions\",\n\
         \x20       \"org.freedesktop.consolekit.system.restart\",\n\
         \x20       \"org.freedesktop.consolekit.system.restart-multiple-users\",\n\
         \x20       \"org.freedesktop.consolekit.system.stop\",\n\
         \x20       \"org.freedesktop.consolekit.system.stop-multiple-users\"\n\
         \x20   ];\n\
         \x20   if (power.indexOf(action.id) >= 0 && subject.user == \"{user}\") {{\n\
         \x20       return polkit.Result.YES;\n\
         \x20   }}\n\
         }});\n"
    )
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
    // So next time the login manager allows it and sudo isn't needed.
    if let Some(user) = args.iter().position(|a| a == "--user").and_then(|i| args.get(i + 1))
        && let Err(e) = install_polkit_rule(user) {
            warn!("No polkit rule for {user}: {e}");
        }
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

#[cfg(test)]
mod tests {
    /// Prints the rule: `cargo test polkit_rule -- --nocapture`
    #[test]
    fn polkit_rule() {
        let rule = super::rule_text("bman");
        println!("{rule}");
        assert!(rule.contains("subject.user == \"bman\""));
        assert!(rule.contains("org.freedesktop.login1.power-off"));
    }
}
