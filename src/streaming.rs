//! Streaming services (Streaming menu). So far: YouTube Music through
//! yt-dlp, which needs Deno for YouTube's JavaScript challenges.
//!
//! This is the one place bman runs programs it didn't write. They're opt-in:
//! "Set up" downloads the official yt-dlp and Deno builds from their GitHub
//! releases into `tools/`, checks their SHA-256 sums and tries a search and
//! a stream before the service can be used. yt-dlp updates itself once a day.
//!
//! A song is played by downloading its whole audio (AAC in M4A, which the
//! normal decoder plays) into `temp/`, then playing that file. The song
//! after it is fetched while it plays. Files are deleted once their song is
//! neither playing nor next, and `temp/` is emptied at startup (in case bman
//! crashed) and on exit.
//!
//! With Videos on, the 144p H.264 video (about 2-3 MB a song) downloads
//! in the same yt-dlp run right after the audio: the song starts as soon as
//! its audio is in, and the video shows up on Now Playing when it's done.
//!
//! Liked songs: bman keeps its own list in `liked.txt` (no login). The
//! YouTube account's own likes (the "Liked music" playlist) can be listed
//! too, with a `youtube-cookies.txt` exported from a browser that's signed
//! in to YouTube (the only login yt-dlp still supports).

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::library::Song;

/// Downloaded programs (yt-dlp, Deno, yt-dlp's cache).
const TOOLS_DIR: &str = "tools";
/// Songs being played / up next. Emptied at startup and exit.
pub const TEMP_DIR: &str = "temp";
/// Album shown for a streamed song until its details are known.
const ALBUM: &str = "YouTube Music";
const SEARCH_RESULTS: usize = 20;
/// bman's own liked songs: "<id>\t<title>" lines, newest first.
const LIKED_FILE: &str = "liked.txt";
/// The account's likes as last loaded, so they show straight away (and
/// offline, or after the login expired). Same format as liked.txt.
const ACCOUNT_LIKES_FILE: &str = "youtube-likes.txt";
/// Netscape cookies.txt from a browser signed in to YouTube.
pub const COOKIES_FILE: &str = "youtube-cookies.txt";
/// The account's "Liked music" playlist.
const ACCOUNT_LIKES_URL: &str = "https://www.youtube.com/playlist?list=LM";
/// The likes are reloaded this often in the background (first a few
/// minutes after startup), so the saved list stays current and the login
/// in the cookies file keeps being used (yt-dlp writes back what YouTube
/// renews).
const ACCOUNT_REFRESH: Duration = Duration::from_secs(6 * 3600);
const ACCOUNT_REFRESH_FIRST: Duration = Duration::from_secs(180);
/// Most account likes listed.
const ACCOUNT_LIKES_MAX: usize = 500;
/// yt-dlp has to keep up with YouTube, so it updates itself this often.
const SELF_UPDATE_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// Biggest tool download accepted (Deno's zip is ~45 MB).
const MAX_DOWNLOAD: u64 = 200 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub enum ToolState {
    /// Not set up yet.
    Missing,
    /// Setting up / testing: what it's doing.
    Working(String),
    Ready,
    /// Set up but the last setup or test failed.
    Broken(String),
    /// No Deno build for this CPU (e.g. 32-bit ARM).
    Unsupported,
}

#[derive(Clone)]
pub struct SearchItem {
    pub id: String,
    pub title: String,
}

/// Details yt-dlp reports once a song is downloaded.
#[derive(Clone)]
pub struct Meta {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
}

pub enum StreamEvent {
    /// The song's file is in temp/ and can be played.
    Ready { id: String, meta: Option<Meta> },
    /// Its video is in temp/ too (comes after Ready).
    VideoReady { id: String },
    Failed { id: String, error: String },
}

/// How far the download of a song's audio has got.
pub struct Progress {
    /// 0.0-1.0, once yt-dlp knows the size (None while it's starting up).
    pub fraction: Option<f32>,
    /// Estimated seconds until it can play.
    pub eta: Option<u64>,
}

/// The audio download now running.
struct Current {
    id: String,
    started: Instant,
    /// Bytes done and total, from yt-dlp's progress lines.
    bytes: Option<(u64, u64)>,
    eta: Option<u64>,
    audio_done: bool,
}

#[derive(Default)]
struct Shared {
    tools: Option<ToolState>,
    searching: bool,
    search_status: String,
    results: Vec<SearchItem>,
    liked: Vec<SearchItem>,
    loading_account: bool,
    account_status: String,
    account: Vec<SearchItem>,
    /// Songs waiting to download, front first.
    queue: VecDeque<String>,
    /// Song downloading now, and its process group (to stop it).
    downloading: Option<(String, i32)>,
    /// A download stopped on purpose (its failure isn't an error).
    stopped: Option<String>,
    events: Vec<StreamEvent>,
    /// Also download videos (Videos: On).
    video: bool,
    current: Option<Current>,
    /// Typical seconds from starting yt-dlp until a song's audio is in
    /// (asking YouTube, the wait it asks for, the download), for the
    /// estimate before yt-dlp reports any progress.
    ready_secs: Option<f32>,
}

#[derive(Clone)]
pub struct YouTube {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    base: PathBuf,
}

impl YouTube {
    /// Empties temp/, finds out whether the tools are there, updates
    /// yt-dlp if it's been a day, and starts the download thread.
    pub fn new() -> Self {
        Self::at(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    /// With tools/ and temp/ in `base` (bman's folder).
    fn at(base: PathBuf) -> Self {
        let yt = Self {
            shared: Arc::new((Mutex::new(Shared::default()), Condvar::new())),
            base,
        };
        yt.purge_temp();
        {
            let mut s = yt.lock();
            s.liked = load_liked(&yt.base.join(LIKED_FILE));
            s.account = load_liked(&yt.base.join(ACCOUNT_LIKES_FILE));
            if !s.account.is_empty() {
                s.account_status = format!("{} liked songs (saved)", s.account.len());
            }
        }
        let state = if !supported() {
            ToolState::Unsupported
        } else if yt.yt_dlp().is_some() && yt.deno().is_file() {
            ToolState::Ready
        } else {
            ToolState::Missing
        };
        yt.lock().tools = Some(state.clone());
        if state == ToolState::Ready {
            yt.self_update_if_due();
        }

        let worker = yt.clone();
        thread::Builder::new()
            .name("bman-stream".into())
            .spawn(move || worker.download_loop())
            .ok();
        let keeper = yt.clone();
        thread::Builder::new()
            .name("bman-yt-likes".into())
            .spawn(move || {
                crate::sched::background();
                thread::sleep(ACCOUNT_REFRESH_FIRST);
                loop {
                    if keeper.has_cookies() && keeper.state() == ToolState::Ready {
                        keeper.load_account_likes();
                    }
                    thread::sleep(ACCOUNT_REFRESH);
                }
            })
            .ok();
        yt
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn state(&self) -> ToolState {
        self.lock().tools.clone().unwrap_or(ToolState::Missing)
    }

    fn set_state(&self, state: ToolState) {
        self.lock().tools = Some(state);
    }

    pub fn take_events(&self) -> Vec<StreamEvent> {
        std::mem::take(&mut self.lock().events)
    }

    // --- paths ----------------------------------------------------------

    fn tools_dir(&self) -> PathBuf {
        self.base.join(TOOLS_DIR)
    }

    fn temp_dir(&self) -> PathBuf {
        self.base.join(TEMP_DIR)
    }

    fn deno(&self) -> PathBuf {
        self.tools_dir().join("deno")
    }

    /// Where a song's audio goes. Streamed songs in the queue use this as
    /// their path; it only exists while the song is playing or next.
    pub fn song_path(&self, id: &str) -> PathBuf {
        self.temp_dir().join(format!("{id}.m4a"))
    }

    /// Where a song's video goes (only with Videos on, and once it's done).
    pub fn video_path(&self, id: &str) -> PathBuf {
        self.temp_dir().join(format!("{id}.mp4"))
    }

    /// Videos: On/Off (downloads after this use it).
    pub fn set_video(&self, on: bool) {
        self.lock().video = on;
    }

    /// Download progress of `id`'s audio, if that's what's downloading.
    pub fn progress(&self, id: &str) -> Option<Progress> {
        let s = self.lock();
        let c = s.current.as_ref().filter(|c| c.id == id && !c.audio_done)?;
        Some(match c.bytes {
            Some((done, total)) if total > 0 => Progress {
                fraction: Some((done as f32 / total as f32).clamp(0.0, 1.0)),
                eta: c.eta,
            },
            // Still starting: guess from how long songs took before.
            _ => {
                let typical = s.ready_secs.unwrap_or(READY_GUESS);
                let left = typical - c.started.elapsed().as_secs_f32();
                Progress { fraction: None, eta: Some(left.max(1.0).ceil() as u64) }
            }
        })
    }

    /// The video id if `path` is a streamed song.
    pub fn id_of(&self, path: &Path) -> Option<String> {
        if path.parent()? != self.temp_dir() || path.extension()? != "m4a" {
            return None;
        }
        Some(path.file_stem()?.to_string_lossy().into_owned()).filter(|id| valid_id(id))
    }

    pub fn song(&self, item: &SearchItem) -> Song {
        Song {
            title: item.title.clone(),
            track: None,
            path: self.song_path(&item.id),
            artist: None,
            album: ALBUM.to_string(),
        }
    }

    /// The yt-dlp command: the small Python build when there's a Python
    /// 3.10+ to run it, otherwise the standalone binary.
    fn yt_dlp(&self) -> Option<Command> {
        let zipapp = self.tools_dir().join("yt-dlp");
        let binary = self.tools_dir().join("yt-dlp-bin");
        if zipapp.is_file() {
            let mut cmd = Command::new("python3");
            cmd.arg(zipapp);
            Some(cmd)
        } else if binary.is_file() {
            Some(Command::new(binary))
        } else {
            None
        }
    }

    /// yt-dlp with the options every run uses.
    fn command(&self) -> Result<Command, String> {
        let mut cmd = self.yt_dlp().ok_or("yt-dlp isn't set up")?;
        cmd.arg("--ignore-config")
            .arg("--no-playlist")
            .args(["--color", "never"])
            .arg("--cache-dir")
            .arg(self.tools_dir().join("cache"))
            .arg("--js-runtimes")
            .arg(format!("deno:{}", self.deno().display()))
            .current_dir(&self.base)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group, so stopping it also stops Deno.
            .process_group(0);
        Ok(cmd)
    }

    // --- set up ----------------------------------------------------------

    /// Download yt-dlp and Deno, then try a search and a stream.
    pub fn setup(&self) {
        if matches!(self.state(), ToolState::Working(_) | ToolState::Unsupported) {
            return;
        }
        let yt = self.clone();
        thread::spawn(move || {
            crate::sched::background();
            let state = match yt.install().and_then(|()| yt.test()) {
                Ok(()) => {
                    info!("YouTube Music is ready");
                    ToolState::Ready
                }
                Err(e) => {
                    warn!("YouTube Music setup failed: {e}");
                    ToolState::Broken(e)
                }
            };
            yt.set_state(state);
        });
    }

    fn install(&self) -> Result<(), String> {
        let (deno_target, ytdlp_binary) = targets().ok_or("Not available on this CPU")?;
        let dir = self.tools_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("Can't make {TOOLS_DIR}/: {e}"))?;
        let agent = agent();

        self.set_state(ToolState::Working("Getting Deno...".into()));
        let base = "https://github.com/denoland/deno/releases/latest/download";
        let zip_name = format!("deno-{deno_target}.zip");
        let expected = sha_for(&get_text(&agent, &format!("{base}/{zip_name}.sha256sum"))?, &zip_name)?;
        let zip_path = dir.join(".deno.zip");
        let result = download(&agent, &format!("{base}/{zip_name}"), &zip_path, &expected)
            .and_then(|()| extract_deno(&zip_path, &self.deno()));
        let _ = fs::remove_file(&zip_path);
        result?;

        self.set_state(ToolState::Working("Getting yt-dlp...".into()));
        let base = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
        let sums = get_text(&agent, &format!("{base}/SHA2-256SUMS"))?;
        let (asset, file, other) = if has_python() {
            ("yt-dlp", "yt-dlp", "yt-dlp-bin")
        } else {
            (ytdlp_binary, "yt-dlp-bin", "yt-dlp")
        };
        let expected = sha_for(&sums, asset)?;
        let tmp = dir.join(".yt-dlp.download");
        download(&agent, &format!("{base}/{asset}"), &tmp, &expected)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        fs::rename(&tmp, dir.join(file)).map_err(|e| e.to_string())?;
        let _ = fs::remove_file(dir.join(other)); // the build we don't use
        touch(&dir.join(".updated"));
        Ok(())
    }

    /// A search, then getting a stream for the first result (this is what
    /// needs Deno, and what breaks when YouTube changes something).
    fn test(&self) -> Result<(), String> {
        self.set_state(ToolState::Working("Testing search...".into()));
        let results = self.run_search("never gonna give you up", false)?;
        let first = results.first().ok_or("Search found nothing")?;
        self.set_state(ToolState::Working("Testing stream...".into()));
        let output = self
            .command()?
            .args(["-f", AUDIO_FORMAT, "--simulate", "--print", "%(format_id)s", "--"])
            .arg(watch_url(&first.id))
            .output()
            .map_err(|e| format!("Can't run yt-dlp: {e}"))?;
        if !output.status.success() {
            return Err(last_error(&output.stderr));
        }
        Ok(())
    }

    /// `yt-dlp -U` once a day, in the background.
    fn self_update_if_due(&self) {
        let stamp = self.tools_dir().join(".updated");
        let due = fs::metadata(&stamp)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_none_or(|age| age >= SELF_UPDATE_INTERVAL);
        if !due {
            return;
        }
        let yt = self.clone();
        thread::spawn(move || {
            crate::sched::background();
            let Ok(mut cmd) = yt.command() else { return };
            match cmd.arg("-U").output() {
                Ok(out) if out.status.success() => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    info!("yt-dlp: {}", text.lines().last().unwrap_or("checked for updates"));
                    touch(&stamp);
                }
                Ok(out) => warn!("yt-dlp update failed: {}", last_error(&out.stderr)),
                Err(e) => warn!("yt-dlp update failed: {e}"),
            }
        });
    }

    // --- search ----------------------------------------------------------

    /// Search YouTube Music's songs, or its videos (`videos`).
    pub fn search(&self, query: String, videos: bool) {
        {
            let mut s = self.lock();
            if s.searching {
                return;
            }
            s.searching = true;
            s.results.clear();
            s.search_status = "Searching...".into();
        }
        let yt = self.clone();
        thread::spawn(move || {
            crate::sched::background();
            let result = yt.run_search(&query, videos);
            let mut s = yt.lock();
            s.searching = false;
            match result {
                Ok(results) => {
                    s.search_status = match results.len() {
                        0 => format!("Nothing found for {query}"),
                        n => format!("{n} {} for {query}", if videos { "videos" } else { "songs" }),
                    };
                    s.results = results;
                }
                Err(e) => {
                    warn!("YouTube Music search failed: {e}");
                    s.search_status = e;
                }
            }
        });
    }

    pub fn results(&self) -> Vec<SearchItem> {
        self.lock().results.clone()
    }

    pub fn search_status(&self) -> String {
        self.lock().search_status.clone()
    }

    fn run_search(&self, query: &str, videos: bool) -> Result<Vec<SearchItem>, String> {
        let section = if videos { "videos" } else { "songs" };
        let url = format!("https://music.youtube.com/search?q={}#{section}", url_encode(query));
        self.list(self.command()?, &url, SEARCH_RESULTS)
    }

    /// The songs of a search or playlist page (no details per song, so it's quick).
    fn list(&self, mut cmd: Command, url: &str, max: usize) -> Result<Vec<SearchItem>, String> {
        let output = cmd
            .args(["--flat-playlist", "--playlist-end", &max.to_string()])
            .args(["--print", "%(id)s\t%(title)s\t%(channel,uploader|)s", "--"])
            .arg(url)
            .output()
            .map_err(|e| format!("Can't run yt-dlp: {e}"))?;
        if !output.status.success() {
            return Err(last_error(&output.stderr));
        }
        Ok(String::from_utf8_lossy(&output.stdout).lines().filter_map(parse_item).collect())
    }

    // --- liked songs -------------------------------------------------------

    /// bman's own liked songs, newest first.
    pub fn liked(&self) -> Vec<SearchItem> {
        self.lock().liked.clone()
    }

    /// Like a song (goes to the top). False if it already was.
    pub fn like(&self, item: &SearchItem) -> bool {
        let mut s = self.lock();
        if s.liked.iter().any(|l| l.id == item.id) {
            return false;
        }
        s.liked.insert(0, item.clone());
        self.save_liked(&s.liked);
        true
    }

    pub fn unlike(&self, id: &str) {
        let mut s = self.lock();
        s.liked.retain(|l| l.id != id);
        self.save_liked(&s.liked);
    }

    fn save_liked(&self, liked: &[SearchItem]) {
        self.save_list(LIKED_FILE, liked);
    }

    /// Write a song list ("<id>\t<title>" lines) to `file` in bman's folder.
    fn save_list(&self, file: &str, items: &[SearchItem]) {
        let text: String = items
            .iter()
            .map(|l| format!("{}\t{}\n", l.id, l.title.replace(['\t', '\n'], " ")))
            .collect();
        let path = self.base.join(file);
        let tmp = path.with_extension("txt.tmp");
        let result = fs::write(&tmp, text).and_then(|()| fs::rename(&tmp, &path));
        if let Err(e) = result {
            warn!("Couldn't save {file}: {e}");
        }
    }

    /// Whether a cookies file is there to list the account's likes with.
    pub fn has_cookies(&self) -> bool {
        self.base.join(COOKIES_FILE).is_file()
    }

    /// Load the account's "Liked music" playlist in the background.
    pub fn load_account_likes(&self) {
        {
            let mut s = self.lock();
            if s.loading_account {
                return;
            }
            s.loading_account = true;
            s.account_status = if s.account.is_empty() { "Loading...".into() } else { "Updating...".into() };
        }
        let yt = self.clone();
        thread::spawn(move || {
            crate::sched::background();
            let result = yt.run_account_likes();
            let mut s = yt.lock();
            s.loading_account = false;
            match result {
                Ok(items) => {
                    s.account_status = match items.len() {
                        0 => "No liked songs".into(),
                        n => format!("{n} liked songs"),
                    };
                    yt.save_list(ACCOUNT_LIKES_FILE, &items);
                    s.account = items;
                }
                Err(e) => {
                    warn!("YouTube likes failed: {e}");
                    // Keep showing the saved list.
                    s.account_status = if s.account.is_empty() { e } else { format!("Saved list. {e}") };
                }
            }
        });
    }

    fn run_account_likes(&self) -> Result<Vec<SearchItem>, String> {
        let cookies = self.base.join(COOKIES_FILE);
        if !cookies.is_file() {
            return Err(format!("No {COOKIES_FILE}"));
        }
        // It's a login: keep it private.
        let _ = fs::set_permissions(&cookies, fs::Permissions::from_mode(0o600));
        let mut cmd = self.command()?;
        cmd.arg("--cookies").arg(&cookies);
        let items = self.list(cmd, ACCOUNT_LIKES_URL, ACCOUNT_LIKES_MAX).map_err(|e| {
            // What a signed-out "Liked music" looks like.
            if e.contains("does not exist") || e.contains("unavailable") {
                "Not signed in: export the cookies again".to_string()
            } else {
                e
            }
        })?;
        Ok(items)
    }

    pub fn account_likes(&self) -> Vec<SearchItem> {
        self.lock().account.clone()
    }

    /// Whether there's a saved copy of the account's likes.
    pub fn has_saved_account_likes(&self) -> bool {
        !self.lock().account.is_empty()
    }

    pub fn account_status(&self) -> String {
        self.lock().account_status.clone()
    }

    // --- downloads ---------------------------------------------------------

    /// Get a song into temp/. `now`: it's wanted to play right away, so it
    /// goes first and stops a download of anything else.
    pub fn fetch(&self, id: &str, now: bool) {
        if self.song_path(id).is_file() {
            let mut s = self.lock();
            s.events.push(StreamEvent::Ready { id: id.to_string(), meta: None });
            if self.video_path(id).is_file() {
                s.events.push(StreamEvent::VideoReady { id: id.to_string() });
            }
            return;
        }
        let mut s = self.lock();
        let busy_with = s.downloading.as_ref().map(|(d, pgid)| (d.clone(), *pgid));
        if busy_with.as_ref().is_some_and(|(d, _)| d == id) {
            return;
        }
        if now {
            // Whatever was waiting was fetched ahead for a song that isn't next any more.
            s.queue.clear();
            s.queue.push_front(id.to_string());
            if let Some((other, pgid)) = busy_with {
                info!("Stopping download of {other} for {id}");
                s.stopped = Some(other);
                unsafe {
                    libc::kill(-pgid, libc::SIGTERM);
                }
            }
        } else if !s.queue.iter().any(|q| q == id) {
            s.queue.push_back(id.to_string());
        }
        self.shared.1.notify_one();
    }

    /// Delete downloaded songs except `keep` (the playing and next ones).
    pub fn clean(&self, keep: &[String]) {
        let downloading = self.lock().downloading.as_ref().map(|(id, _)| id.clone());
        let Ok(entries) = fs::read_dir(self.temp_dir()) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name.split('.').next().unwrap_or_default();
            if keep.iter().any(|k| k == id) || downloading.as_deref() == Some(id) {
                continue;
            }
            let _ = fs::remove_file(entry.path());
        }
    }

    /// Stop any download and empty temp/ (startup and exit).
    pub fn purge_temp(&self) {
        if let Some((_, pgid)) = self.lock().downloading.take() {
            unsafe {
                libc::kill(-pgid, libc::SIGTERM);
            }
        }
        let _ = fs::remove_dir_all(self.temp_dir());
    }

    fn download_loop(&self) {
        crate::sched::background();
        loop {
            let id = {
                let mut s = self.lock();
                loop {
                    if let Some(id) = s.queue.pop_front() {
                        break id;
                    }
                    s = self.shared.1.wait(s).unwrap_or_else(|e| e.into_inner());
                }
            };
            let video = self.lock().video;
            let mut result = self.download(&id, video);
            // A video can lack the 144p H.264 track: get the audio alone.
            if video && result.as_ref().is_err_and(|e| e.contains("format is not available")) {
                result = self.download(&id, false);
            }
            let mut s = self.lock();
            if let Err(error) = result {
                if s.stopped.as_deref() != Some(id.as_str()) {
                    warn!("Couldn't stream {id}: {error}");
                }
                s.events.push(StreamEvent::Failed { id, error });
            }
            s.stopped = None;
            s.downloading = None;
            s.current = None;
        }
    }

    /// The audio is in: tell the app, and give a liked song its full title.
    fn audio_ready(&self, id: &str, meta: Option<Meta>) {
        let mut s = self.lock();
        if let Some(c) = s.current.as_mut() {
            c.audio_done = true;
            let took = c.started.elapsed().as_secs_f32().min(120.0);
            s.ready_secs = Some(s.ready_secs.map_or(took, |avg| avg * 0.7 + took * 0.3));
        }
        if let Some(m) = &meta {
            let title = match &m.artist {
                Some(a) => format!("{a} - {}", m.title),
                None => m.title.clone(),
            };
            let mut changed = false;
            for l in s.liked.iter_mut().filter(|l| l.id == id && l.title != title) {
                l.title = title.clone();
                changed = true;
            }
            if changed {
                self.save_liked(&s.liked);
            }
        }
        s.events.push(StreamEvent::Ready { id: id.to_string(), meta });
    }

    /// Run yt-dlp for a song: its audio, then (with `video`) its 144p video.
    /// Ok once the audio is in, even if the video then fails.
    fn download(&self, id: &str, video: bool) -> Result<(), String> {
        let temp = self.temp_dir();
        fs::create_dir_all(&temp).map_err(|e| format!("Can't make {TEMP_DIR}/: {e}"))?;
        let formats = if video { format!("{AUDIO_FORMAT},{VIDEO_FORMAT}") } else { AUDIO_FORMAT.to_string() };
        let mut child = self
            .command()?
            .args(["-f", &formats, "--no-simulate"])
            .args(["--progress", "--newline", "--progress-delta", "1"])
            .args(["--progress-template", "download:P %(progress.downloaded_bytes)s %(progress.total_bytes,progress.total_bytes_estimate)s %(progress.eta)s %(info.ext)s"])
            .arg("-o")
            .arg(temp.join(format!("{id}.%(ext)s")))
            .arg("--write-thumbnail")
            .arg("-o")
            .arg(format!("thumbnail:{}", temp.join(id).display()))
            .args(["--print", "after_move:D %(ext)s\t%(track,title)s\t%(artist,creator,uploader)s\t%(album)s", "--"])
            .arg(watch_url(id))
            .spawn()
            .map_err(|e| format!("Can't run yt-dlp: {e}"))?;
        {
            let mut s = self.lock();
            s.downloading = Some((id.to_string(), child.id() as i32));
            s.current = Some(Current { id: id.to_string(), started: Instant::now(), bytes: None, eta: None, audio_done: false });
        }

        // Read stderr on its own thread so a chatty yt-dlp can't block.
        let stderr = child.stderr.take();
        let errors = thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut e) = stderr {
                let _ = e.read_to_end(&mut text);
            }
            text
        });
        let mut audio_done = false;
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix("P ") {
                    // "P <done> <total> <eta> <ext>", for the audio only.
                    let f: Vec<&str> = rest.split(' ').collect();
                    if f.get(3) == Some(&"m4a") {
                        let num = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).map(|v| v as u64);
                        if let Some(c) = self.lock().current.as_mut() {
                            c.bytes = num(0).zip(num(1));
                            c.eta = num(2);
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("D ") {
                    let mut fields = rest.split('\t');
                    match fields.next() {
                        Some("m4a") if !audio_done => {
                            audio_done = true;
                            let mut fields = fields.map(|f| {
                                let f = f.trim();
                                (!f.is_empty() && f != "NA").then(|| f.to_string())
                            });
                            let title = fields.next().flatten();
                            let artist = fields.next().flatten();
                            let album = fields.next().flatten();
                            self.audio_ready(id, title.map(|title| Meta { title, artist, album }));
                        }
                        Some("mp4") if self.video_path(id).is_file() => {
                            self.lock().events.push(StreamEvent::VideoReady { id: id.to_string() });
                        }
                        _ => {}
                    }
                }
            }
        }
        let status = child.wait().map_err(|e| e.to_string())?;
        let stderr = errors.join().unwrap_or_default();

        if audio_done && self.song_path(id).is_file() {
            if !status.success() && video && !self.video_path(id).is_file() {
                info!("No video for {id}: {}", last_error(&stderr));
            }
            return Ok(());
        }
        // Stopped for another song (or failed): don't leave half a file.
        for entry in fs::read_dir(&temp).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().starts_with(&format!("{id}.")) {
                let _ = fs::remove_file(entry.path());
            }
        }
        Err(if status.success() { "Download failed".into() } else { last_error(&stderr) })
    }
}

/// AAC audio (M4A), which Symphonia decodes. YouTube's Opus streams it can't.
const AUDIO_FORMAT: &str = "140/bestaudio[ext=m4a]";
/// 144p H.264 video (240p if there's no 144p), which video.rs decodes.
const VIDEO_FORMAT: &str = "160/133";
/// Seconds a song is assumed to take to be ready, until one has been timed.
const READY_GUESS: f32 = 10.0;

fn watch_url(id: &str) -> String {
    format!("https://music.youtube.com/watch?v={id}")
}

/// YouTube video ids: 11 of [A-Za-z0-9_-].
fn valid_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// (Deno target, standalone yt-dlp asset) for this CPU. Deno has no 32-bit builds.
fn targets() -> Option<(&'static str, &'static str)> {
    match std::env::consts::ARCH {
        "x86_64" => Some(("x86_64-unknown-linux-gnu", "yt-dlp_linux")),
        "aarch64" => Some(("aarch64-unknown-linux-gnu", "yt-dlp_linux_aarch64")),
        _ => None,
    }
}

fn supported() -> bool {
    targets().is_some()
}

/// A Python new enough for yt-dlp (3.10+). Its small zip build starts much
/// faster than the standalone binary, which unpacks itself on every run.
fn has_python() -> bool {
    Command::new("python3")
        .args(["-c", "import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The last "ERROR:" line yt-dlp printed (or its last line), shortened.
fn last_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .or_else(|| text.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("yt-dlp failed");
    let line = line.trim_start_matches("ERROR:").trim();
    // "[youtube] <id>: Sign in to confirm..." -> "Sign in to confirm..."
    let line = match line.split_once(": ") {
        Some((head, rest)) if valid_id(head.rsplit(' ').next().unwrap_or("")) => rest,
        _ => line,
    };
    line.chars().take(80).collect()
}

/// "<id>\t<title>[\t<channel>]" -> item, titled "Artist - Title" when the
/// channel is an artist's ("Name - Topic" channels are YouTube Music's).
fn parse_item(line: &str) -> Option<SearchItem> {
    let mut parts = line.split('\t');
    let id = parts.next()?.trim();
    let title = parts.next()?.trim();
    let channel = parts.next().unwrap_or("").trim();
    if !valid_id(id) || title.is_empty() {
        return None;
    }
    let artist = channel.strip_suffix(" - Topic").filter(|a| !a.is_empty());
    Some(SearchItem {
        id: id.to_string(),
        title: match artist {
            Some(a) if !title.contains(a) => format!("{a} - {title}"),
            _ => title.to_string(),
        },
    })
}

fn load_liked(path: &Path) -> Vec<SearchItem> {
    fs::read_to_string(path)
        .map(|text| text.lines().filter_map(parse_item).collect())
        .unwrap_or_default()
}

fn url_encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn touch(path: &Path) {
    let _ = fs::write(path, b"");
}

// --- downloading the tools --------------------------------------------------

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(600)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .user_agent(concat!("bman/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn network_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Io(_) | ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => "No internet".into(),
        ureq::Error::Timeout(_) => "Timed out".into(),
        e => e.to_string(),
    }
}

fn get_text(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    agent
        .get(url)
        .call()
        .map_err(network_error)?
        .body_mut()
        .read_to_string()
        .map_err(network_error)
}

/// The hash for `name` from a sha256sum-style file ("<hash>  <name>" lines,
/// or just a hash).
fn sha_for(sums: &str, name: &str) -> Result<String, String> {
    sums.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let hash = parts.next()?;
            let file = parts.next().map(|f| f.trim_start_matches('*'));
            (file.is_none() || file == Some(name)).then_some(hash)
        })
        .find(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|h| h.to_ascii_lowercase())
        .ok_or_else(|| format!("No checksum for {name}"))
}

/// Download `url` to `path` (streamed, not held in memory), checking its SHA-256.
fn download(agent: &ureq::Agent, url: &str, path: &Path, expected: &str) -> Result<(), String> {
    let mut response = agent.get(url).call().map_err(network_error)?;
    let mut reader = response.body_mut().with_config().limit(MAX_DOWNLOAD).reader();
    let mut file = fs::File::create(path).map_err(|e| format!("Can't write {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let result = (|| -> io::Result<()> {
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
        }
        file.sync_all()
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(path);
        return Err(format!("Download failed: {e}"));
    }
    let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        let _ = fs::remove_file(path);
        return Err("Download corrupted (checksum)".into());
    }
    Ok(())
}

/// Unpack the `deno` binary from Deno's release zip.
fn extract_deno(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let file = fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("Bad Deno zip: {e}"))?;
    let mut entry = archive.by_name("deno").map_err(|e| format!("Bad Deno zip: {e}"))?;
    let tmp = dest.with_extension("tmp");
    let result = (|| -> io::Result<()> {
        let mut out = fs::File::create(&tmp)?;
        io::copy(&mut entry, &mut out)?;
        out.sync_all()?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        fs::rename(&tmp, dest)
    })();
    result.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("Can't unpack Deno: {e}")
    })
}

#[cfg(test)]
mod live {
    use super::*;
    use std::time::Instant;

    fn wait_for(what: &str, secs: u64, mut done: impl FnMut() -> bool) {
        let t = Instant::now();
        while !done() {
            assert!(t.elapsed() < Duration::from_secs(secs), "{what} took too long");
            thread::sleep(Duration::from_millis(200));
        }
        println!("{what}: {:.1?}", t.elapsed());
    }

    /// Sets up, searches and streams a song for real, in BMAN_TEST_DIR:
    /// `BMAN_TEST_DIR=/tmp/x cargo test youtube_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn youtube_live() {
        let dir = PathBuf::from(std::env::var("BMAN_TEST_DIR").expect("BMAN_TEST_DIR"));
        fs::create_dir_all(&dir).unwrap();
        let yt = YouTube::at(dir.clone());
        if yt.state() != ToolState::Ready {
            yt.setup();
            let mut last = String::new();
            wait_for("setup", 900, || {
                let state = yt.state();
                if let ToolState::Working(w) = &state {
                    if *w != last {
                        println!("  {w}");
                        last = w.clone();
                    }
                }
                !matches!(state, ToolState::Working(_) | ToolState::Missing)
            });
            match yt.state() {
                ToolState::Broken(e) => panic!("setup failed: {e}"),
                s => assert!(s == ToolState::Ready),
            }
        }

        yt.search("daft punk get lucky".into(), false);
        wait_for("search", 120, || !yt.lock().searching);
        let results = yt.results();
        println!("  {}", yt.search_status());
        for r in results.iter().take(3) {
            println!("  {} {}", r.id, r.title);
        }
        let (a, b) = (&results[0], &results[1]);

        // Fetch one ahead, then ask for another now: the first is stopped.
        yt.fetch(&a.id, false);
        thread::sleep(Duration::from_secs(2));
        yt.fetch(&b.id, true);
        let mut events = Vec::new();
        wait_for("download", 300, || {
            events.extend(yt.take_events());
            events.iter().any(|e| matches!(e, StreamEvent::Ready { id, .. } if *id == b.id))
        });
        for e in &events {
            match e {
                StreamEvent::Ready { id, meta } => println!(
                    "  ready {id}: {:?}",
                    meta.as_ref().map(|m| (&m.title, &m.artist, &m.album))
                ),
                StreamEvent::Failed { id, error } => println!("  failed {id}: {error}"),
                StreamEvent::VideoReady { id } => println!("  video {id}"),
            }
        }
        let path = yt.song_path(&b.id);
        assert_eq!(yt.id_of(&path).as_deref(), Some(b.id.as_str()));
        println!("  files: {:?}", fs::read_dir(dir.join(TEMP_DIR)).unwrap().flatten().map(|e| e.file_name()).collect::<Vec<_>>());
        assert!(path.is_file());

        yt.clean(&[]);
        assert!(!path.exists());
        yt.purge_temp();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums() {
        let h = "a".repeat(64);
        let sums = format!("{h}  yt-dlp\n{}  yt-dlp_linux\n", "b".repeat(64));
        assert_eq!(sha_for(&sums, "yt-dlp").unwrap(), h);
        assert_eq!(sha_for(&sums, "yt-dlp_linux").unwrap(), "b".repeat(64));
        assert!(sha_for(&sums, "nope").is_err());
        assert_eq!(sha_for(&h, "anything").unwrap(), h);
    }

    #[test]
    fn errors() {
        let e = b"WARNING: x\nERROR: [youtube] dQw4w9WgXcQ: Sign in to confirm you're not a bot\n";
        assert_eq!(last_error(e), "Sign in to confirm you're not a bot");
        assert_eq!(url_encode("a b&c"), "a+b%26c");
        assert!(valid_id("dQw4w9WgXcQ"));
        assert!(!valid_id("../etc/pass"));
    }

    #[test]
    fn items() {
        let i = parse_item("dQw4w9WgXcQ\tNever Gonna Give You Up\tRick Astley - Topic").unwrap();
        assert_eq!(i.title, "Rick Astley - Never Gonna Give You Up");
        let i = parse_item("dQw4w9WgXcQ\tNever Gonna Give You Up\tNA").unwrap();
        assert_eq!(i.title, "Never Gonna Give You Up");
        assert_eq!(parse_item("dQw4w9WgXcQ\tSong").unwrap().title, "Song");
        assert!(parse_item("bad\tSong").is_none());
    }

    #[test]
    fn liked_file() {
        let dir = std::env::temp_dir().join(format!("bman-liked-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let yt = YouTube { shared: Default::default(), base: dir.clone() };
        let a = SearchItem { id: "aaaaaaaaaaa".into(), title: "A".into() };
        let b = SearchItem { id: "bbbbbbbbbbb".into(), title: "B\tx".into() };
        assert!(yt.like(&a));
        assert!(yt.like(&b));
        assert!(!yt.like(&a));
        let loaded = load_liked(&dir.join(LIKED_FILE));
        assert_eq!(loaded.iter().map(|l| l.title.as_str()).collect::<Vec<_>>(), ["B x", "A"]);
        yt.unlike("bbbbbbbbbbb");
        assert_eq!(load_liked(&dir.join(LIKED_FILE)).len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
