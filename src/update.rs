//! Self-updater: checks the GitHub releases of the repository in Cargo.toml,
//! downloads the build for this machine, checks its SHA-256, swaps it in
//! place of the running binary and asks main to restart.
//!
//! Releases are made by .github/workflows/release.yml when a `vX.Y.Z` tag is
//! pushed. Each release has one binary per target, `bman-<target>`, plus a
//! `bman-<target>.sha256` next to it.
//!
//! Everything is pure Rust (ureq + rustls with bundled root certificates), so
//! it doesn't depend on curl, wget or the system's certificate store.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// The version this binary was built as.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Automatic checks run at startup and then this often. The check is one
/// small request to github.com (not the rate-limited API), so once a minute
/// is fine.
const CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Biggest download accepted. The release binary is a few MB.
const MAX_DOWNLOAD: u64 = 64 * 1024 * 1024;

/// Name of the release asset built for this machine, or None if releases
/// aren't built for it (e.g. a musl system).
pub fn asset_name() -> Option<&'static str> {
    if cfg!(not(target_os = "linux")) || cfg!(not(target_env = "gnu")) {
        return None;
    }
    match std::env::consts::ARCH {
        "x86_64" => Some("bman-x86_64-linux-gnu"),
        "aarch64" => Some("bman-aarch64-linux-gnu"),
        "arm" => Some("bman-armv7-linux-gnueabihf"),
        _ => None,
    }
}

/// "owner/repo" from the `repository` key in Cargo.toml.
fn repo() -> Option<&'static str> {
    env!("CARGO_PKG_REPOSITORY")
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .strip_prefix("https://github.com/")
}

/// "v1.2.3" or "1.2.3" -> (1, 2, 3). Anything after a '-' or '+' is ignored.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches('v');
    let text = text.split(['-', '+']).next()?;
    let mut parts = text.split('.').map(|p| p.parse::<u64>());
    let major = parts.next()?.ok()?;
    let minor = parts.next().unwrap_or(Ok(0)).ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

fn is_newer(tag: &str) -> bool {
    match (parse_version(tag), parse_version(VERSION)) {
        (Some(new), Some(current)) => new > current,
        _ => false,
    }
}

#[derive(Clone)]
pub struct Release {
    /// e.g. "v0.2.0"
    pub tag: String,
    repo: &'static str,
    /// This machine's build, e.g. "bman-aarch64-linux-gnu".
    asset: &'static str,
}

impl Release {
    fn url(&self, name: &str) -> String {
        format!("https://github.com/{}/releases/download/{}/{name}", self.repo, self.tag)
    }
}

#[derive(Clone)]
pub enum UpdateState {
    /// Nothing checked yet.
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Installing,
    /// The new binary is in place; main should restart into it.
    Installed,
    Failed,
}

/// What happened in the background since the last call to `take_events`.
pub enum UpdateEvent {
    /// A newer release was found (by an automatic or manual check).
    Found(String),
    UpToDate,
    Failed(String),
    /// Installed; restart to run it.
    Installed(String),
}

struct Shared {
    state: UpdateState,
    events: Vec<UpdateEvent>,
    /// The last check was started by a button press (so "up to date" and
    /// errors are worth a toast; automatic checks stay quiet).
    manual: bool,
    /// Last error logged, so a check failing every minute logs once.
    last_error: Option<String>,
    /// Why the last install failed (shown on System until the next try).
    install_error: Option<String>,
}

pub struct Updater {
    shared: Arc<Mutex<Shared>>,
    auto_check: bool,
    next_auto_check: Instant,
}

impl Updater {
    pub fn new(auto_check: bool) -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared {
                state: UpdateState::Idle,
                events: Vec::new(),
                manual: false,
                last_error: None,
                install_error: None,
            })),
            auto_check,
            next_auto_check: Instant::now(),
        }
    }

    pub fn state(&self) -> UpdateState {
        self.shared.lock().unwrap().state.clone()
    }

    fn is_busy(&self) -> bool {
        matches!(
            self.shared.lock().unwrap().state,
            UpdateState::Checking | UpdateState::Installing
        )
    }

    pub fn take_events(&self) -> Vec<UpdateEvent> {
        std::mem::take(&mut self.shared.lock().unwrap().events)
    }

    /// Why the last install failed, until the next try.
    pub fn install_error(&self) -> Option<String> {
        self.shared.lock().unwrap().install_error.clone()
    }

    pub fn auto_check(&self) -> bool {
        self.auto_check
    }

    pub fn set_auto_check(&mut self, on: bool) {
        self.auto_check = on;
        self.next_auto_check = Instant::now();
    }

    /// Call once per frame: runs the automatic checks.
    pub fn poll(&mut self) {
        if !self.auto_check || Instant::now() < self.next_auto_check {
            return;
        }
        // Don't auto-check over a result the user is looking at or acting on.
        let state = self.state();
        match state {
            UpdateState::Available(_) | UpdateState::Installed => {
                self.next_auto_check = Instant::now() + CHECK_INTERVAL;
            }
            _ if self.is_busy() => {}
            _ => {
                self.check(false);
                self.next_auto_check = Instant::now() + CHECK_INTERVAL;
            }
        }
    }

    /// Look for a newer release on a background thread.
    pub fn check(&mut self, manual: bool) {
        if self.is_busy() {
            return;
        }
        {
            let mut shared = self.shared.lock().unwrap();
            shared.state = UpdateState::Checking;
            shared.manual = manual;
        }

        let shared = Arc::clone(&self.shared);
        thread::spawn(move || {
            crate::sched::background(); // never compete with playback
            let result = latest_release();
            let mut shared = shared.lock().unwrap();
            let manual = shared.manual;
            match result {
                Ok(Some(release)) => {
                    info!("Update available: {} (running v{VERSION})", release.tag);
                    shared.events.push(UpdateEvent::Found(release.tag.clone()));
                    shared.state = UpdateState::Available(release);
                }
                Ok(_) => {
                    shared.state = UpdateState::UpToDate;
                    shared.last_error = None;
                    if manual {
                        shared.events.push(UpdateEvent::UpToDate);
                    }
                }
                Err(e) => {
                    if shared.last_error.as_ref() != Some(&e) {
                        warn!("Update check failed: {e}");
                        shared.last_error = Some(e.clone());
                    }
                    shared.state = UpdateState::Failed;
                    if manual {
                        shared.events.push(UpdateEvent::Failed(short_error(&format!("Update check failed\n{e}"))));
                    }
                }
            }
        });

        self.next_auto_check = Instant::now() + CHECK_INTERVAL;
    }

    /// Download and install the release found by the last check.
    pub fn install(&mut self) {
        let UpdateState::Available(release) = self.state() else {
            return;
        };
        {
            let mut shared = self.shared.lock().unwrap();
            shared.state = UpdateState::Installing;
            shared.install_error = None;
        }

        let shared = Arc::clone(&self.shared);
        thread::spawn(move || {
            crate::sched::background(); // never compete with playback
            let result = install(&release);
            let mut shared = shared.lock().unwrap();
            match result {
                Ok(()) => {
                    info!("Installed {}", release.tag);
                    shared.state = UpdateState::Installed;
                    shared.events.push(UpdateEvent::Installed(release.tag));
                }
                Err(e) => {
                    error!("Update download failed: {e}");
                    // Keep the release so another press can retry.
                    shared.state = UpdateState::Available(release);
                    shared.events.push(UpdateEvent::Failed(short_error(&format!("Download failed\n{e}"))));
                    shared.install_error = Some(e);
                }
            }
        });
    }
}

/// Up to two lines, each cut down to fit a toast.
fn short_error(e: &str) -> String {
    e.lines()
        .take(2)
        .map(|line| {
            if line.chars().count() > 26 {
                let cut: String = line.chars().take(25).collect();
                format!("{cut}…")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .user_agent(concat!("bman/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// The latest published release, if it's newer and has a build for this
/// machine. `github.com/<repo>/releases/latest` redirects to
/// `.../releases/tag/<tag>`, so the tag comes from the redirect without
/// touching the rate-limited API, and the download URLs follow from it.
fn latest_release() -> Result<Option<Release>, String> {
    let repo = repo().ok_or("No GitHub repo set")?;
    let asset = asset_name().ok_or("No builds for this system")?;

    let no_redirects: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent(concat!("bman/", env!("CARGO_PKG_VERSION")))
        .max_redirects(0)
        .max_redirects_will_error(false)
        .build()
        .into();
    let url = format!("https://github.com/{repo}/releases/latest");
    let response = no_redirects.head(&url).call().map_err(network_error)?;

    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    // With no releases it redirects to the releases page instead.
    let Some(tag) = location.split("/releases/tag/").nth(1).filter(|t| !t.is_empty()) else {
        return Err("No releases yet".into());
    };
    let tag = tag.to_string();
    if !is_newer(&tag) {
        return Ok(None);
    }

    let release = Release { tag, repo, asset };
    // Make sure this release has our build (it could still be uploading, or
    // the target was dropped).
    match agent().head(&release.url(&format!("{asset}.sha256"))).call() {
        Ok(_) => Ok(Some(release)),
        Err(ureq::Error::StatusCode(404)) => Ok(None),
        // GitHub's own trouble; installing falls back to the API.
        Err(ureq::Error::StatusCode(code)) if code >= 500 => Ok(Some(release)),
        Err(e) => Err(network_error(e)),
    }
}

fn network_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Io(_) | ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => {
            "No internet".to_string()
        }
        ureq::Error::Timeout(_) => "Timed out, GitHub may be down".to_string(),
        // GitHub's side (502/503/504 when it's having trouble).
        ureq::Error::StatusCode(code) if code >= 500 => format!("GitHub may be down ({code})"),
        ureq::Error::StatusCode(code) => format!("GitHub said {code}"),
        e => e.to_string(),
    }
}

fn install(release: &Release) -> Result<(), String> {
    let agent = agent();

    let mut api_assets = None;
    // Expected hash: the first word of the .sha256 file (sha256sum format).
    let sha_text = download(&agent, release, &format!("{}.sha256", release.asset), &mut api_assets)?;
    let sha_text = String::from_utf8_lossy(&sha_text);
    let expected = sha_text
        .split_whitespace()
        .next()
        .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or("Bad checksum file")?
        .to_ascii_lowercase();

    let binary = download(&agent, release, release.asset, &mut api_assets)?;

    let actual: String = Sha256::digest(&binary)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != expected {
        return Err("Download corrupted (checksum)".to_string());
    }
    // Quick sanity check that it's a Linux executable at all.
    if !binary.starts_with(b"\x7fELF") {
        return Err("Download isn't a program".to_string());
    }

    let exe = current_exe()?;
    let tmp = exe.with_file_name(".bman-update.tmp");
    let write = || -> std::io::Result<()> {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&binary)?;
        file.sync_all()?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        // Replacing a running binary by rename is safe on Linux: the running
        // process keeps the old file open until it exits.
        fs::rename(&tmp, &exe)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            "Can't write program folder".to_string()
        } else {
            format!("Install failed: {e}")
        }
    })
}

/// A file of the release. github.com's download links can fail on GitHub's
/// side (504s that lasted for hours on bman's repo while the API served the
/// same files), so on a server error or timeout it's fetched through the API
/// instead. `api_assets`: the release's (name, asset id) list, looked up once.
fn download(
    agent: &ureq::Agent,
    release: &Release,
    name: &str,
    api_assets: &mut Option<Vec<(String, u64)>>,
) -> Result<Vec<u8>, String> {
    let read = |req: ureq::RequestBuilder<ureq::typestate::WithoutBody>| {
        req.call()?.body_mut().with_config().limit(MAX_DOWNLOAD).read_to_vec()
    };
    match read(agent.get(&release.url(name))) {
        Ok(bytes) => return Ok(bytes),
        Err(ureq::Error::StatusCode(code)) if code >= 500 => {
            warn!("Download of {name} failed (HTTP {code}), trying the GitHub API");
        }
        Err(ureq::Error::Timeout(_)) => warn!("Download of {name} timed out, trying the GitHub API"),
        Err(e) => return Err(network_error(e)),
    }
    if api_assets.is_none() {
        let url = format!("https://api.github.com/repos/{}/releases/tags/{}", release.repo, release.tag);
        let json = agent
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(network_error)?
            .body_mut()
            .read_to_string()
            .map_err(network_error)?;
        *api_assets = Some(parse_assets(&json));
    }
    let id = api_assets
        .as_ref()
        .and_then(|list| list.iter().find(|(n, _)| n == name))
        .map(|(_, id)| *id)
        .ok_or_else(|| format!("{name} isn't in the release"))?;
    let url = format!("https://api.github.com/repos/{}/releases/assets/{id}", release.repo);
    read(agent.get(&url).header("Accept", "application/octet-stream")).map_err(network_error)
}

/// (name, id) of each asset in a GitHub release JSON. Each asset object
/// starts `"url": ".../releases/assets/<id>"` and its `"name"` comes next.
fn parse_assets(json: &str) -> Vec<(String, u64)> {
    const MARK: &str = "/releases/assets/";
    let mut out = Vec::new();
    let mut rest = json;
    while let Some(at) = rest.find(MARK) {
        rest = &rest[at + MARK.len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let Ok(id) = digits.parse() else { continue };
        let Some(key) = rest.find("\"name\"") else { break };
        let after = rest[key + 6..].trim_start().strip_prefix(':').unwrap_or("").trim_start();
        if let Some(value) = after.strip_prefix('"').and_then(|v| v.split('"').next()) {
            out.push((value.to_string(), id));
        }
    }
    out
}

/// Path of the running binary. After an update replaced it, Linux reports
/// "<path> (deleted)", so strip that.
fn current_exe() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Can't find program: {e}"))?;
    let text = exe.to_string_lossy();
    Ok(match text.strip_suffix(" (deleted)") {
        Some(path) => PathBuf::from(path),
        None => exe,
    })
}

/// Replace this process with the (new) binary, same arguments and working
/// directory. Only returns if that failed.
pub fn restart() -> String {
    use std::os::unix::process::CommandExt;
    let exe = match current_exe() {
        Ok(exe) => exe,
        Err(e) => return e,
    };
    let err = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec();
    format!("Restart failed: {err}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.2"), Some((0, 2, 0)));
        assert_eq!(parse_version("v1.0.0-beta"), Some((1, 0, 0)));
        assert_eq!(parse_version("nope"), None);
        assert!((0, 10, 0) > (0, 9, 9));
    }

    /// Talks to GitHub: `cargo test live_check -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_check() {
        match latest_release() {
            Ok(Some(r)) => println!("latest {} newer={}", r.tag, is_newer(&r.tag)),
            Ok(None) => println!("latest release has no build for {:?}", asset_name()),
            Err(e) => println!("error: {e}"),
        }
    }

    #[test]
    fn assets_from_json() {
        let json = r#"{"assets_url": "https://api.github.com/repos/a/b/releases/1/assets",
          "assets": [
            {"url": "https://api.github.com/repos/a/b/releases/assets/11", "id": 11, "name": "bman-x86_64-linux-gnu"},
            {"url":"https://api.github.com/repos/a/b/releases/assets/12","id":12,"name" : "bman-x86_64-linux-gnu.sha256"}
          ]}"#;
        assert_eq!(
            parse_assets(json),
            vec![("bman-x86_64-linux-gnu".to_string(), 11), ("bman-x86_64-linux-gnu.sha256".to_string(), 12)]
        );
    }

    #[test]
    fn toast_text() {
        let e = network_error(ureq::Error::StatusCode(504));
        assert_eq!(short_error(&format!("Download failed\n{e}")), "Download failed\nGitHub may be down (504)");
    }

    #[test]
    fn repo_from_cargo() {
        assert_eq!(repo(), Some("birdguy-cs/bman"));
    }

    /// Downloads this machine's build of v0.4.0 (network; not installed).
    #[test]
    #[ignore]
    fn live_download() {
        let release = Release { tag: "v0.4.0".into(), repo: repo().unwrap(), asset: asset_name().unwrap() };
        let mut api = None;
        let sha = download(&agent(), &release, &format!("{}.sha256", release.asset), &mut api).unwrap();
        let bin = download(&agent(), &release, release.asset, &mut api).unwrap();
        let actual: String = Sha256::digest(&bin).iter().map(|b| format!("{b:02x}")).collect();
        assert!(String::from_utf8_lossy(&sha).starts_with(&actual));
        assert!(bin.starts_with(b"\x7fELF"));
    }
}
