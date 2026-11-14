//! Podcasts and audiobooks: spoken audio, which (unlike music) remembers
//! where you stopped, can play faster (stretch.rs) and skips 30 s at a time.
//!
//! Folders beside `music/` (also shown over USB transfer):
//! - `audiobooks/`: a folder per book (its files are the chapters, in name
//!   order) or a single file (`.m4b` chapters are read from the file).
//!   Books can be downloaded from LibriVox (free, public domain).
//! - `podcasts/`: a folder per show. Shows come from feeds: `feeds.txt` in
//!   there (one URL per line, editable from a computer over USB) or the
//!   Apple Podcasts directory search. A refresh downloads each show's newest
//!   episode; older ones download when picked. Files you copy in count too.
//!
//! `progress.txt` in bman's folder keeps each file's position, whether it
//! was finished, and when it was last played (`path\tsecs\tdone\twhen`).
//!
//! Network: ureq (pure Rust TLS) like the updater; RSS through quick-xml,
//! the search APIs' JSON through serde_json. Downloads go to a hidden
//! `.bman-download-*` file and are renamed when complete, so neither the
//! library nor a computer on USB sees half a file.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime};

pub const PODCASTS_DIR: &str = "podcasts";
pub const AUDIOBOOKS_DIR: &str = "audiobooks";
/// Subscriptions, one feed URL per line, inside podcasts/.
pub const FEEDS_FILE: &str = "feeds.txt";
const PROGRESS_FILE: &str = "progress.txt";
/// In a show's folder: its feed URL, and the episodes the feed listed
/// (`date\tguid\ttitle\turl\tfile`). Hidden, so USB and the scan skip them.
const SHOW_FEED: &str = ".feed";
const SHOW_EPISODES: &str = ".episodes";
const DOWNLOAD_PREFIX: &str = ".bman-download-";
/// Audio files that count as episodes / chapters.
const EXTENSIONS: &[&str] = &["mp3", "m4a", "m4b", "aac", "ogg", "flac", "wav", "opus"];
/// Feeds are refreshed this long after startup, then this often.
pub const REFRESH_FIRST: Duration = Duration::from_secs(90);
pub const REFRESH_EVERY: Duration = Duration::from_secs(6 * 3600);
/// Speeds 5 on Now Playing steps through.
pub const SPEEDS: [f64; 6] = [1.0, 1.25, 1.5, 1.75, 2.0, 0.75];
/// Closer than this to the end counts as finished.
const DONE_MARGIN: u64 = 30;
const USER_AGENT: &str = concat!("bman/", env!("CARGO_PKG_VERSION"), " (podcast player)");

// --- library ------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Book {
    pub title: String,
    pub author: Option<String>,
    /// The book's folder, or its single file.
    pub path: PathBuf,
    /// Audio files in order (one for a single-file book).
    pub files: Vec<PathBuf>,
}

impl Book {
    pub fn label(&self) -> String {
        match &self.author {
            Some(a) => format!("{} - {a}", self.title),
            None => self.title.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Episode {
    pub title: String,
    /// "2024-11-05", or empty for a file with no date.
    pub date: String,
    /// Where the file is (or will be once downloaded).
    pub path: PathBuf,
    /// Feed episodes: where to download it from.
    pub url: Option<String>,
}

impl Episode {
    pub fn downloaded(&self) -> bool {
        self.path.is_file()
    }
}

#[derive(Clone, Debug)]
pub struct Show {
    pub title: String,
    pub dir: PathBuf,
    pub feed: Option<String>,
    /// Newest first.
    pub episodes: Vec<Episode>,
}

#[derive(Clone, Debug)]
pub struct Chapter {
    pub title: String,
    pub file: PathBuf,
    pub start: Duration,
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| EXTENSIONS.contains(&e.to_string_lossy().to_lowercase().as_str()))
}

fn hidden(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

/// Sorted entries of a folder, without hidden ones.
fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| !hidden(p)).collect())
        .unwrap_or_default();
    v.sort_by(|a, b| crate::library::natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    v
}

fn audio_files(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    for p in entries(dir) {
        if p.is_dir() && depth < 4 {
            audio_files(&p, out, depth + 1);
        } else if p.is_file() && is_audio(&p) {
            out.push(p);
        }
    }
}

/// "Author - Title" (folder or file name) into (title, author).
fn split_name(name: &str) -> (String, Option<String>) {
    match name.split_once(" - ") {
        Some((author, title)) if !author.trim().is_empty() && !title.trim().is_empty() => {
            (title.trim().to_string(), Some(author.trim().to_string()))
        }
        _ => (name.trim().to_string(), None),
    }
}

/// Books in `audiobooks/` (`base` = bman's folder, or the music root's
/// parent).
pub fn scan_books(dir: &Path) -> Vec<Book> {
    let mut books = Vec::new();
    for p in entries(dir) {
        let stem = if p.is_dir() {
            p.file_name().unwrap_or_default().to_string_lossy().into_owned()
        } else {
            p.file_stem().unwrap_or_default().to_string_lossy().into_owned()
        };
        let mut files = Vec::new();
        if p.is_dir() {
            audio_files(&p, &mut files, 0);
        } else if is_audio(&p) {
            files.push(p.clone());
        }
        if files.is_empty() {
            continue;
        }
        let (title, author) = split_name(&stem);
        books.push(Book { title, author, path: p, files });
    }
    books
}

/// Shows in `podcasts/`: a folder each, with the feed's episode list and
/// whatever audio files are in it.
pub fn scan_shows(dir: &Path) -> Vec<Show> {
    let mut shows = Vec::new();
    for p in entries(dir).into_iter().filter(|p| p.is_dir()) {
        let feed = fs::read_to_string(p.join(SHOW_FEED)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let mut episodes: Vec<Episode> = fs::read_to_string(p.join(SHOW_EPISODES))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                (f.len() >= 5).then(|| Episode {
                    date: f[0].to_string(),
                    title: f[2].to_string(),
                    url: Some(f[3].to_string()).filter(|u| !u.is_empty()),
                    path: p.join(f[4]),
                })
            })
            .collect();
        let mut local = Vec::new();
        audio_files(&p, &mut local, 0);
        for file in local {
            if episodes.iter().any(|e| e.path == file) {
                continue;
            }
            let stem = file.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            let (date, title) = match stem.split_once(" - ") {
                Some((d, t)) if is_date(d) => (d.to_string(), t.to_string()),
                _ => (String::new(), stem),
            };
            episodes.push(Episode { title, date, path: file, url: None });
        }
        // Newest first; undated files after, by name.
        episodes.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.title.cmp(&b.title)));
        let title = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
        shows.push(Show { title, dir: p, feed, episodes });
    }
    shows
}

fn is_date(s: &str) -> bool {
    s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-'
}

/// A path is podcast or audiobook audio (gets speed, resume and skips).
pub fn is_spoken(path: &Path) -> bool {
    path.components().any(|c| {
        let c = c.as_os_str();
        c == PODCASTS_DIR || c == AUDIOBOOKS_DIR
    })
}

/// The chapters of a book: its files, or a single file's own chapters.
pub fn chapters(book: &Book) -> Vec<Chapter> {
    if let [file] = book.files.as_slice()
        && let Some(list) = mp4_chapters(file).filter(|l| l.len() > 1) {
            return list
                .into_iter()
                .map(|(start, title)| Chapter { title, file: file.clone(), start })
                .collect();
        }
    book.files
        .iter()
        .map(|f| Chapter {
            title: f.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
            file: f.clone(),
            start: Duration::ZERO,
        })
        .collect()
}

// --- progress -------------------------------------------------------------------

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Mark {
    pub secs: u64,
    pub done: bool,
    /// Unix time of the last save, to find where a book was left.
    pub when: u64,
}

pub struct Progress {
    path: PathBuf,
    marks: HashMap<PathBuf, Mark>,
    dirty: bool,
}

impl Progress {
    pub fn load(base: &Path) -> Self {
        let path = base.join(PROGRESS_FILE);
        let marks = fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                let mark = Mark {
                    secs: f.get(1)?.parse().ok()?,
                    done: f.get(2) == Some(&"1"),
                    when: f.get(3).and_then(|w| w.parse().ok()).unwrap_or(0),
                };
                Some((PathBuf::from(f[0]), mark))
            })
            .collect();
        Self { path, marks, dirty: false }
    }

    pub fn get(&self, file: &Path) -> Mark {
        self.marks.get(file).copied().unwrap_or_default()
    }

    /// Where to start a file: its saved position, unless it was finished.
    pub fn resume_at(&self, file: &Path) -> Duration {
        let m = self.get(file);
        if m.done { Duration::ZERO } else { Duration::from_secs(m.secs.saturating_sub(2)) }
    }

    /// Save a position (`length` 0 = unknown). Near the end counts as done.
    pub fn set(&mut self, file: &Path, secs: u64, length: u64) {
        let done = length > 0 && secs + DONE_MARGIN >= length && length > DONE_MARGIN;
        let when = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mark = Mark { secs, done: done || self.get(file).done && secs < 5, when };
        if self.marks.get(file) != Some(&mark) {
            self.marks.insert(file.to_path_buf(), mark);
            self.dirty = true;
        }
    }

    pub fn set_done(&mut self, file: &Path, done: bool) {
        let mut mark = self.get(file);
        mark.done = done;
        if done {
            mark.secs = 0;
        }
        self.marks.insert(file.to_path_buf(), mark);
        self.dirty = true;
    }

    /// The file of `files` played last and its position, if any was played.
    pub fn last_of(&self, files: &[PathBuf]) -> Option<(usize, Duration)> {
        files
            .iter()
            .enumerate()
            .filter_map(|(i, f)| self.marks.get(f).map(|m| (i, m)))
            .filter(|(_, m)| m.when > 0)
            .max_by_key(|(_, m)| m.when)
            .map(|(i, m)| {
                if m.done && i + 1 < files.len() {
                    (i + 1, Duration::ZERO) // that chapter's finished: the next one
                } else {
                    (i, if m.done { Duration::ZERO } else { Duration::from_secs(m.secs.saturating_sub(2)) })
                }
            })
    }

    /// How far through a book is (0-100), by files finished.
    pub fn percent(&self, files: &[PathBuf]) -> Option<u8> {
        let started = files.iter().any(|f| self.marks.contains_key(f));
        if !started {
            return None;
        }
        let done = files.iter().filter(|f| self.get(f).done).count();
        Some((done * 100 / files.len().max(1)) as u8)
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let mut lines: Vec<String> = self
            .marks
            .iter()
            .filter(|(p, _)| p.exists())
            .map(|(p, m)| format!("{}\t{}\t{}\t{}\n", p.display(), m.secs, u8::from(m.done), m.when))
            .collect();
        lines.sort();
        let tmp = self.path.with_extension("txt.tmp");
        let result = fs::write(&tmp, lines.concat()).and_then(|()| fs::rename(&tmp, &self.path));
        match result {
            Ok(()) => self.dirty = false,
            Err(e) => warn!("Couldn't save {}: {e}", self.path.display()),
        }
    }
}

// --- network: feeds, search, downloads ---------------------------------------------------

/// A podcast or a LibriVox book found by a search.
#[derive(Clone, Debug)]
pub struct Found {
    pub title: String,
    pub by: String,
    /// Podcast: the feed URL. LibriVox: unused.
    pub feed: String,
    /// LibriVox: (chapter title, mp3 URL).
    pub chapters: Vec<(String, String)>,
}

impl Found {
    pub fn label(&self) -> String {
        match (self.by.is_empty(), self.chapters.is_empty()) {
            (true, _) => self.title.clone(),
            (false, true) => format!("{} - {}", self.title, self.by),
            (false, false) => format!("{} - {} ({} ch)", self.title, self.by, self.chapters.len()),
        }
    }
}

#[derive(Default)]
struct Shared {
    /// Downloads waiting: (url, file).
    queue: std::collections::VecDeque<(String, PathBuf)>,
    /// The file downloading now and how far (0-100) it got.
    current: Option<(PathBuf, u8)>,
    /// Files that finished (Ok) or failed (Err) since the last look.
    finished: Vec<(PathBuf, Result<(), String>)>,
    podcast_results: Vec<Found>,
    book_results: Vec<Found>,
    /// What the last search / refresh is doing or found.
    status: String,
    busy: bool,
    /// Files changed (rescan).
    changed: bool,
}

/// Handle to the background network work.
#[derive(Clone)]
pub struct Spoken {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    /// bman's folder: podcasts/ and audiobooks/ are in it.
    base: PathBuf,
}

impl Spoken {
    pub fn new(base: PathBuf) -> Self {
        let spoken = Self { shared: Arc::new((Mutex::new(Shared::default()), Condvar::new())), base };
        // Leftovers of a download cut off by a crash.
        for dir in [spoken.podcasts(), spoken.audiobooks()] {
            remove_partials(&dir, 0);
        }
        let worker = spoken.clone();
        thread::Builder::new().name("bman-spoken".into()).spawn(move || worker.download_loop()).ok();
        spoken
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn podcasts(&self) -> PathBuf {
        self.base.join(PODCASTS_DIR)
    }

    pub fn audiobooks(&self) -> PathBuf {
        self.base.join(AUDIOBOOKS_DIR)
    }

    pub fn status(&self) -> String {
        self.lock().status.clone()
    }

    pub fn busy(&self) -> bool {
        self.lock().busy
    }

    pub fn podcast_results(&self) -> Vec<Found> {
        self.lock().podcast_results.clone()
    }

    pub fn book_results(&self) -> Vec<Found> {
        self.lock().book_results.clone()
    }

    /// Files were added (rescan the library); clears the flag.
    pub fn take_changed(&self) -> bool {
        std::mem::take(&mut self.lock().changed)
    }

    /// Downloads finished since the last call.
    pub fn take_finished(&self) -> Vec<(PathBuf, Result<(), String>)> {
        std::mem::take(&mut self.lock().finished)
    }

    /// Download progress of `file`: Some(percent) while queued or loading.
    pub fn progress(&self, file: &Path) -> Option<u8> {
        let s = self.lock();
        match &s.current {
            Some((f, pct)) if f == file => Some(*pct),
            _ => s.queue.iter().any(|(_, f)| f == file).then_some(0),
        }
    }

    pub fn download(&self, url: &str, file: &Path) {
        let mut s = self.lock();
        let busy = s.current.as_ref().is_some_and(|(f, _)| f == file) || s.queue.iter().any(|(_, f)| f == file);
        if !busy && !file.is_file() {
            s.queue.push_back((url.to_string(), file.to_path_buf()));
            self.shared.1.notify_one();
        }
    }

    /// Run `job` on a thread with `busy` and a status line.
    fn job(&self, status: &str, job: impl FnOnce(&Spoken) -> Result<String, String> + Send + 'static) {
        {
            let mut s = self.lock();
            if s.busy {
                return;
            }
            s.busy = true;
            s.status = status.to_string();
        }
        let me = self.clone();
        thread::spawn(move || {
            crate::sched::background();
            let result = job(&me);
            let mut s = me.lock();
            s.busy = false;
            s.status = match result {
                Ok(msg) => msg,
                Err(e) => {
                    warn!("{e}");
                    e
                }
            };
        });
    }

    /// Search the Apple Podcasts directory (free, no account).
    pub fn search_podcasts(&self, term: String) {
        self.job("Searching...", move |me| {
            let url = format!(
                "https://itunes.apple.com/search?media=podcast&entity=podcast&limit=25&term={}",
                url_encode(&term)
            );
            let json = get_json(&url)?;
            let found: Vec<Found> = json["results"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|r| {
                            Some(Found {
                                title: r["collectionName"].as_str()?.to_string(),
                                by: r["artistName"].as_str().unwrap_or("").to_string(),
                                feed: r["feedUrl"].as_str()?.to_string(),
                                chapters: Vec::new(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let n = found.len();
            me.lock().podcast_results = found;
            Ok(if n == 0 { format!("Nothing found for \"{term}\"") } else { format!("{n} podcasts") })
        });
    }

    /// Search LibriVox by title and by author.
    pub fn search_books(&self, term: String) {
        self.job("Searching LibriVox...", move |me| {
            let mut found: Vec<Found> = Vec::new();
            let mut ids = Vec::new();
            let mut last_err = None;
            for key in ["title", "author"] {
                let url = format!(
                    "https://librivox.org/api/feed/audiobooks/?{key}=%5E{}&format=json&extended=1&limit=20",
                    url_encode(&term)
                );
                let json = match get_json(&url) {
                    Ok(j) => j,
                    Err(e) => {
                        last_err = Some(e);
                        continue;
                    }
                };
                for b in json["books"].as_array().into_iter().flatten() {
                    let id = b["id"].as_str().unwrap_or("").to_string();
                    if ids.contains(&id) {
                        continue;
                    }
                    let authors: Vec<String> = b["authors"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|a| {
                            format!("{} {}", a["first_name"].as_str().unwrap_or(""), a["last_name"].as_str().unwrap_or(""))
                                .trim()
                                .to_string()
                        })
                        .collect();
                    let chapters: Vec<(String, String)> = b["sections"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|s| {
                            let url = s["listen_url"].as_str().filter(|u| !u.is_empty())?;
                            Some((s["title"].as_str().unwrap_or("Chapter").to_string(), url.to_string()))
                        })
                        .collect();
                    if chapters.is_empty() {
                        continue;
                    }
                    ids.push(id);
                    found.push(Found {
                        title: b["title"].as_str().unwrap_or("?").trim().to_string(),
                        by: authors.join(", "),
                        feed: String::new(),
                        chapters,
                    });
                }
            }
            if found.is_empty()
                && let Some(e) = last_err {
                    return Err(e);
                }
            let n = found.len();
            me.lock().book_results = found;
            Ok(if n == 0 { format!("Nothing found for \"{term}\"") } else { format!("{n} books") })
        });
    }

    /// Download a LibriVox book into audiobooks/<Author - Title>/.
    pub fn get_book(&self, book: &Found) -> PathBuf {
        let name = match book.by.split(", ").next().filter(|a| !a.is_empty()) {
            Some(author) => format!("{author} - {}", book.title),
            None => book.title.clone(),
        };
        let dir = self.audiobooks().join(safe_name(&name));
        let _ = fs::create_dir_all(&dir);
        for (i, (title, url)) in book.chapters.iter().enumerate() {
            let file = dir.join(format!("{:02} - {}.mp3", i + 1, safe_name(title)));
            self.download(url, &file);
        }
        self.lock().status = format!("Downloading {} chapters", book.chapters.len());
        dir
    }

    /// Add a feed to feeds.txt and fetch it.
    pub fn subscribe(&self, feed: &str) {
        let file = self.podcasts().join(FEEDS_FILE);
        let _ = fs::create_dir_all(self.podcasts());
        let text = fs::read_to_string(&file).unwrap_or_default();
        if !feed_urls(&text).iter().any(|u| u == feed) {
            let sep = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
            let header = if text.is_empty() { "# Podcast feeds bman follows: one URL per line.\n" } else { "" };
            if let Err(e) = fs::write(&file, format!("{header}{text}{sep}{feed}\n")) {
                warn!("Couldn't save {}: {e}", file.display());
            }
        }
        let feed = feed.to_string();
        self.job("Adding podcast...", move |me| me.refresh_feed(&feed).map(|t| format!("Added {t}")));
    }

    /// Take a feed out of feeds.txt (its folder and files stay).
    pub fn unsubscribe(&self, feed: &str) {
        let file = self.podcasts().join(FEEDS_FILE);
        let Ok(text) = fs::read_to_string(&file) else { return };
        let kept: String = text.lines().filter(|l| l.trim() != feed).map(|l| format!("{l}\n")).collect();
        if let Err(e) = fs::write(&file, kept) {
            warn!("Couldn't save {}: {e}", file.display());
        }
        let show_feed = self.find_show_dir(feed).map(|d| d.join(SHOW_FEED));
        if let Some(f) = show_feed {
            let _ = fs::remove_file(f);
        }
        self.lock().changed = true;
    }

    /// Fetch every feed in feeds.txt.
    pub fn refresh(&self) {
        self.job("Refreshing podcasts...", |me| {
            let text = fs::read_to_string(me.podcasts().join(FEEDS_FILE)).unwrap_or_default();
            let urls = feed_urls(&text);
            if urls.is_empty() {
                return Ok("No podcasts yet".into());
            }
            let mut errors = 0;
            for url in &urls {
                if let Err(e) = me.refresh_feed(url) {
                    warn!("Feed {url}: {e}");
                    errors += 1;
                }
            }
            Ok(match errors {
                0 => format!("{} podcasts up to date", urls.len()),
                n => format!("{n} of {} feeds failed", urls.len()),
            })
        });
    }

    fn find_show_dir(&self, feed: &str) -> Option<PathBuf> {
        entries(&self.podcasts())
            .into_iter()
            .find(|d| fs::read_to_string(d.join(SHOW_FEED)).is_ok_and(|f| f.trim() == feed))
    }

    /// Fetch one feed: write the show's episode list, get its newest episode.
    fn refresh_feed(&self, url: &str) -> Result<String, String> {
        let body = get_text(url)?;
        let feed = parse_feed(&body);
        if feed.title.is_empty() && feed.episodes.is_empty() {
            return Err(format!("Not a podcast feed: {url}"));
        }
        let dir = self
            .find_show_dir(url)
            .unwrap_or_else(|| self.podcasts().join(safe_name(if feed.title.is_empty() { "Podcast" } else { &feed.title })));
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        fs::write(dir.join(SHOW_FEED), format!("{url}\n")).map_err(|e| e.to_string())?;
        let mut list = String::new();
        let mut files = Vec::new();
        for ep in &feed.episodes {
            let file = episode_file_name(ep);
            list.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                ep.date,
                ep.guid.replace(['\t', '\n'], " "),
                ep.title.replace(['\t', '\n'], " "),
                ep.url,
                file
            ));
            files.push((ep.url.clone(), dir.join(file)));
        }
        fs::write(dir.join(SHOW_EPISODES), list).map_err(|e| e.to_string())?;
        // The newest episode, unless it's there already or was played.
        if let Some((url, file)) = files.first() {
            let progress = Progress::load(&self.base);
            if !file.is_file() && !progress.get(file).done {
                self.download(url, file);
            }
        }
        self.lock().changed = true;
        Ok(feed.title)
    }

    fn download_loop(&self) {
        crate::sched::background();
        loop {
            let (url, file) = {
                let mut s = self.lock();
                loop {
                    if let Some(job) = s.queue.pop_front() {
                        s.current = Some((job.1.clone(), 0));
                        break job;
                    }
                    s = self.shared.1.wait(s).unwrap_or_else(|e| e.into_inner());
                }
            };
            let result = self.fetch_file(&url, &file);
            match &result {
                Ok(()) => info!("Downloaded {}", file.display()),
                Err(e) => warn!("Download of {} failed: {e}", file.display()),
            }
            let mut s = self.lock();
            s.current = None;
            s.changed |= result.is_ok();
            s.finished.push((file, result));
            if s.queue.is_empty() && !s.busy {
                s.status = "Downloads done".into();
            }
        }
    }

    fn fetch_file(&self, url: &str, file: &Path) -> Result<(), String> {
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        let tmp = file.with_file_name(format!("{DOWNLOAD_PREFIX}{name}"));
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut response = agent().get(url).call().map_err(net_error)?;
        let total: Option<u64> = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        let mut reader = response.body_mut().as_reader();
        let mut out = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut got = 0u64;
        let result = (|| -> io::Result<()> {
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
                got += n as u64;
                if let Some(t) = total.filter(|&t| t > 0) {
                    let pct = (got * 100 / t).min(99) as u8;
                    if let Some((_, p)) = self.lock().current.as_mut() {
                        *p = pct;
                    }
                }
            }
            out.sync_all()
        })();
        if let Err(e) = result.and_then(|()| fs::rename(&tmp, file)) {
            let _ = fs::remove_file(&tmp);
            return Err(e.to_string());
        }
        Ok(())
    }
}

fn remove_partials(dir: &Path, depth: u32) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if e.file_name().to_string_lossy().starts_with(DOWNLOAD_PREFIX) {
            let _ = fs::remove_file(&p);
        } else if p.is_dir() && depth < 3 {
            remove_partials(&p, depth + 1);
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .user_agent(USER_AGENT)
        .build()
        .into()
}

fn net_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Io(_) | ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => "No internet".into(),
        ureq::Error::Timeout(_) => "Timed out".into(),
        ureq::Error::StatusCode(c) => format!("Server said {c}"),
        e => e.to_string(),
    }
}

fn get_text(url: &str) -> Result<String, String> {
    agent()
        .get(url)
        .call()
        .map_err(net_error)?
        .body_mut()
        .with_config()
        .limit(32 * 1024 * 1024)
        .read_to_string()
        .map_err(net_error)
}

fn get_json(url: &str) -> Result<serde_json::Value, String> {
    let text = get_text(url)?;
    serde_json::from_str(&text).map_err(|e| format!("Bad reply: {e}"))
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Lines of feeds.txt that are URLs.
fn feed_urls(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| l.starts_with("http://") || l.starts_with("https://"))
        .map(str::to_string)
        .collect()
}

/// A name usable as a file or folder name everywhere (FAT, Windows, USB).
pub fn safe_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned = cleaned.trim_start_matches('.').trim_end_matches(['.', ' ']).to_string();
    let cut: String = cleaned.chars().take(100).collect();
    if cut.is_empty() { "Untitled".into() } else { cut }
}

struct FeedEpisode {
    guid: String,
    title: String,
    date: String,
    url: String,
}

struct Feed {
    title: String,
    episodes: Vec<FeedEpisode>,
}

fn episode_file_name(ep: &FeedEpisode) -> String {
    let ext = ep
        .url
        .split(['?', '#'])
        .next()
        .and_then(|u| u.rsplit('/').next())
        .and_then(|f| f.rsplit_once('.').map(|(_, e)| e.to_lowercase()))
        .filter(|e| EXTENSIONS.contains(&e.as_str()))
        .unwrap_or_else(|| "mp3".into());
    let title = safe_name(&ep.title);
    if ep.date.is_empty() { format!("{title}.{ext}") } else { format!("{} - {title}.{ext}", ep.date) }
}

/// RSS: the channel's title, and each item with an audio enclosure.
fn parse_feed(xml: &str) -> Feed {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut feed = Feed { title: String::new(), episodes: Vec::new() };
    let mut path: Vec<String> = Vec::new();
    let mut item: Option<FeedEpisode> = None;
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "item" {
                    item = Some(FeedEpisode { guid: String::new(), title: String::new(), date: String::new(), url: String::new() });
                }
                path.push(name);
                text.clear();
            }
            Ok(Event::Empty(e)) => {
                if e.name().as_ref() == b"enclosure"
                    && let Some(it) = item.as_mut() {
                        for a in e.attributes().flatten() {
                            if a.key.as_ref() == b"url" {
                                it.url = a
                                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                    .map(|v| v.into_owned())
                                    .unwrap_or_default();
                            }
                        }
                    }
            }
            Ok(Event::Text(t)) => {
                if let Ok(t) = t.decode() {
                    text.push_str(&t);
                }
            }
            // &amp; and friends come separately.
            Ok(Event::GeneralRef(r)) => match r.resolve_char_ref() {
                Ok(Some(c)) => text.push(c),
                _ => match r.decode().as_deref() {
                    Ok("amp") => text.push('&'),
                    Ok("lt") => text.push('<'),
                    Ok("gt") => text.push('>'),
                    Ok("quot") => text.push('"'),
                    Ok("apos") => text.push('\''),
                    _ => {}
                },
            },
            Ok(Event::CData(t)) => text.push_str(&String::from_utf8_lossy(&t)),
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let parent = path.len().checked_sub(2).and_then(|i| path.get(i)).map(String::as_str);
                match (name.as_str(), parent, item.as_mut()) {
                    ("title", Some("channel"), _) if feed.title.is_empty() => feed.title = text.trim().to_string(),
                    ("title", Some("item"), Some(it)) => it.title = text.trim().to_string(),
                    ("guid", Some("item"), Some(it)) => it.guid = text.trim().to_string(),
                    ("pubDate", Some("item"), Some(it)) => it.date = rfc2822_date(text.trim()),
                    ("item", _, _) => {
                        if let Some(mut it) = item.take()
                            && !it.url.is_empty() {
                                if it.guid.is_empty() {
                                    it.guid = it.url.clone();
                                }
                                feed.episodes.push(it);
                            }
                    }
                    _ => {}
                }
                path.pop();
                text.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    feed.episodes.sort_by(|a, b| b.date.cmp(&a.date));
    feed
}

/// "Tue, 05 Nov 2024 10:00:00 +0000" -> "2024-11-05" (empty if unreadable).
fn rfc2822_date(s: &str) -> String {
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let words: Vec<&str> = s.split([' ', ',']).filter(|w| !w.is_empty()).collect();
    for i in 0..words.len().saturating_sub(2) {
        let (d, m, y) = (words[i], words[i + 1], words[i + 2]);
        let month = MONTHS.iter().position(|mm| m.len() >= 3 && m[..3].eq_ignore_ascii_case(mm));
        if let (Ok(day), Some(month), Ok(year)) = (d.parse::<u32>(), month, y.parse::<u32>())
            && (1..=31).contains(&day) && year > 1900 {
                return format!("{year:04}-{:02}-{day:02}", month + 1);
            }
    }
    String::new()
}

// --- MP4 chapters (.m4b) ------------------------------------------------------------------

/// Chapters of an MP4/M4B file: Nero `chpl` in moov/udta, else a QuickTime
/// chapter text track (`tref/chap`). (start, title), in order.
pub fn mp4_chapters(path: &Path) -> Option<Vec<(Duration, String)>> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    if !["m4b", "m4a", "mp4", "aac"].contains(&ext.as_str()) {
        return None;
    }
    let mut file = fs::File::open(path).ok()?;
    let moov = read_moov(&mut file)?;
    if let Some(list) = child(&moov, b"udta").and_then(|u| child(u, b"chpl")).and_then(nero_chapters) {
        return Some(list);
    }
    qt_chapters(&moov, &mut file)
}

fn be16(d: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(i..i + 2)?.try_into().ok()?))
}
fn be32(d: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(i..i + 4)?.try_into().ok()?))
}
fn be64(d: &[u8], i: usize) -> Option<u64> {
    Some(u64::from_be_bytes(d.get(i..i + 8)?.try_into().ok()?))
}

/// The boxes in `d`: (type, body).
fn boxes(d: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 <= d.len() {
        let size = be32(d, i).unwrap_or(0) as usize;
        let kind: [u8; 4] = d[i + 4..i + 8].try_into().unwrap_or([0; 4]);
        let (header, size) = match size {
            1 => (16, be64(d, i + 8).unwrap_or(0) as usize),
            0 => (8, d.len() - i),
            s => (8, s),
        };
        if size < header || i + size > d.len() {
            break;
        }
        out.push((kind, &d[i + header..i + size]));
        i += size;
    }
    out
}

fn child<'a>(d: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(d).into_iter().find(|(k, _)| k == kind).map(|(_, b)| b)
}

fn read_moov(file: &mut fs::File) -> Option<Vec<u8>> {
    let len = file.metadata().ok()?.len();
    let mut pos = 0u64;
    while pos + 8 <= len {
        file.seek(SeekFrom::Start(pos)).ok()?;
        let mut h = [0u8; 16];
        file.read_exact(&mut h[..8]).ok()?;
        let mut size = u32::from_be_bytes(h[..4].try_into().ok()?) as u64;
        let mut header = 8;
        if size == 1 {
            file.read_exact(&mut h[8..16]).ok()?;
            size = u64::from_be_bytes(h[8..16].try_into().ok()?);
            header = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < header {
            return None;
        }
        if &h[4..8] == b"moov" {
            if size > 64 * 1024 * 1024 {
                return None;
            }
            let mut body = vec![0u8; (size - header) as usize];
            file.read_exact(&mut body).ok()?;
            return Some(body);
        }
        pos += size;
    }
    None
}

fn nero_chapters(chpl: &[u8]) -> Option<Vec<(Duration, String)>> {
    let version = *chpl.first()?;
    let mut i = 4 + if version > 0 { 4 } else { 0 };
    let count = *chpl.get(i)? as usize;
    i += 1;
    let mut out = Vec::new();
    for _ in 0..count {
        let start = be64(chpl, i)?; // 100 ns units
        let len = *chpl.get(i + 8)? as usize;
        let title = String::from_utf8_lossy(chpl.get(i + 9..i + 9 + len)?).into_owned();
        out.push((Duration::from_nanos(start * 100), title));
        i += 9 + len;
    }
    (!out.is_empty()).then_some(out)
}

fn qt_chapters(moov: &[u8], file: &mut fs::File) -> Option<Vec<(Duration, String)>> {
    let traks: Vec<&[u8]> = boxes(moov).into_iter().filter(|(k, _)| k == b"trak").map(|(_, b)| b).collect();
    let track_id = |t: &[u8]| -> Option<u32> {
        let tkhd = child(t, b"tkhd")?;
        be32(tkhd, if tkhd[0] == 1 { 20 } else { 12 })
    };
    let chap_ids: Vec<u32> = traks
        .iter()
        .filter_map(|t| child(t, b"tref").and_then(|r| child(r, b"chap")))
        .flat_map(|c| (0..c.len() / 4).filter_map(move |i| be32(c, i * 4)))
        .collect();
    let trak = traks.iter().find(|t| track_id(t).is_some_and(|id| chap_ids.contains(&id)))?;
    let mdia = child(trak, b"mdia")?;
    let mdhd = child(mdia, b"mdhd")?;
    let timescale = be32(mdhd, if mdhd[0] == 1 { 20 } else { 12 })?.max(1) as u64;
    let stbl = child(child(mdia, b"minf")?, b"stbl")?;

    // Sample start times.
    let stts = child(stbl, b"stts")?;
    let mut times = Vec::new();
    let mut t = 0u64;
    for e in 0..be32(stts, 4)? as usize {
        let (count, delta) = (be32(stts, 8 + e * 8)?, be32(stts, 12 + e * 8)? as u64);
        for _ in 0..count.min(10_000) {
            times.push(t);
            t += delta;
        }
    }
    // Sample sizes.
    let stsz = child(stbl, b"stsz")?;
    let (fixed, count) = (be32(stsz, 4)?, be32(stsz, 8)? as usize);
    let sizes: Vec<u32> = (0..count.min(10_000)).map(|i| if fixed != 0 { Some(fixed) } else { be32(stsz, 12 + i * 4) }).collect::<Option<_>>()?;
    // Chunk offsets, and how many samples each chunk has.
    let offsets: Vec<u64> = if let Some(stco) = child(stbl, b"stco") {
        (0..be32(stco, 4)? as usize).map(|i| be32(stco, 8 + i * 4).map(u64::from)).collect::<Option<_>>()?
    } else {
        let co64 = child(stbl, b"co64")?;
        (0..be32(co64, 4)? as usize).map(|i| be64(co64, 8 + i * 8)).collect::<Option<_>>()?
    };
    let stsc = child(stbl, b"stsc")?;
    let runs: Vec<(u32, u32)> = (0..be32(stsc, 4)? as usize)
        .map(|e| Some((be32(stsc, 8 + e * 12)?, be32(stsc, 12 + e * 12)?)))
        .collect::<Option<_>>()?;
    let mut sample_offsets = Vec::new();
    for (c, &chunk_off) in offsets.iter().enumerate() {
        let chunk_no = c as u32 + 1;
        let per = runs.iter().rev().find(|(first, _)| *first <= chunk_no).map_or(1, |r| r.1);
        let mut off = chunk_off;
        for _ in 0..per {
            let i = sample_offsets.len();
            let Some(&size) = sizes.get(i) else { break };
            sample_offsets.push(off);
            off += size as u64;
        }
    }

    let mut out = Vec::new();
    for (i, &off) in sample_offsets.iter().enumerate() {
        let size = (sizes[i] as usize).min(4096);
        let mut buf = vec![0u8; size];
        file.seek(SeekFrom::Start(off)).ok()?;
        file.read_exact(&mut buf).ok()?;
        let len = (be16(&buf, 0).unwrap_or(0) as usize).min(size.saturating_sub(2));
        let raw = &buf[2..2 + len];
        let title = if raw.starts_with(&[0xFE, 0xFF]) {
            let units: Vec<u16> = raw[2..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&units)
        } else {
            String::from_utf8_lossy(raw).into_owned()
        };
        let start = Duration::from_millis(times.get(i).copied().unwrap_or(0) * 1000 / timescale);
        out.push((start, title));
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_names() {
        assert_eq!(rfc2822_date("Tue, 05 Nov 2024 10:00:00 +0000"), "2024-11-05");
        assert_eq!(rfc2822_date("5 November 2024"), "2024-11-05");
        assert_eq!(rfc2822_date("garbage"), "");
        assert_eq!(safe_name("a/b: c?  d."), "a b c d");
        assert_eq!(safe_name("..."), "Untitled");
        assert_eq!(split_name("Jane Austen - Emma"), ("Emma".into(), Some("Jane Austen".into())));
        assert_eq!(url_encode("a b&c"), "a+b%26c");
        assert!(is_spoken(Path::new("podcasts/Show/ep.mp3")) && !is_spoken(Path::new("music/a.mp3")));
    }

    #[test]
    fn rss() {
        let xml = r#"<?xml version="1.0"?><rss xmlns:itunes="x"><channel><title>My &amp; Show</title>
            <image><title>not this</title></image>
            <item><title><![CDATA[Ep 2: B/C]]></title><guid>g2</guid><pubDate>Wed, 06 Nov 2024 10:00:00 GMT</pubDate>
              <enclosure url="https://x.test/2.m4a?x=1" type="audio/mp4" length="1"/></item>
            <item><title>Ep 1</title><pubDate>Tue, 05 Nov 2024 10:00:00 GMT</pubDate>
              <enclosure url="https://x.test/1.mp3" length="1" type="audio/mpeg"/></item>
            <item><title>No audio</title></item>
            </channel></rss>"#;
        let f = parse_feed(xml);
        assert_eq!(f.title, "My & Show");
        assert_eq!(f.episodes.len(), 2);
        assert_eq!(f.episodes[0].title, "Ep 2: B/C");
        assert_eq!(episode_file_name(&f.episodes[0]), "2024-11-06 - Ep 2 B C.m4a");
        assert_eq!(f.episodes[1].guid, "https://x.test/1.mp3");
    }

    #[test]
    fn progress_and_resume() {
        let base = std::env::temp_dir().join(format!("bman-progress-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let files: Vec<PathBuf> = (1..=3).map(|i| base.join(format!("{i}.mp3"))).collect();
        for f in &files {
            fs::write(f, "x").unwrap();
        }
        let mut p = Progress::load(&base);
        assert_eq!(p.last_of(&files), None);
        p.set(&files[0], 590, 600); // within 30 s of the end: done
        std::thread::sleep(Duration::from_millis(1100));
        p.set(&files[1], 125, 600);
        p.save();
        let p = Progress::load(&base);
        assert!(p.get(&files[0]).done);
        assert_eq!(p.last_of(&files), Some((1, Duration::from_secs(123))));
        assert_eq!(p.percent(&files), Some(33));
        assert_eq!(p.resume_at(&files[0]), Duration::ZERO);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn nero_chpl() {
        let mut d = vec![1, 0, 0, 0, 0, 0, 0, 0, 2];
        for (t, name) in [(0u64, "One"), (600_000_000, "Two")] {
            d.extend(t.to_be_bytes());
            d.push(name.len() as u8);
            d.extend(name.as_bytes());
        }
        let ch = nero_chapters(&d).unwrap();
        assert_eq!(ch[1], (Duration::from_secs(60), "Two".to_string()));
    }

    /// A real .m4b made by ffmpeg (QuickTime chapter track):
    /// `BMAN_TEST_M4B=file.m4b cargo test m4b_chapters -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn m4b_chapters() {
        let path = PathBuf::from(std::env::var("BMAN_TEST_M4B").expect("BMAN_TEST_M4B"));
        let ch = mp4_chapters(&path).expect("chapters");
        println!("{ch:?}");
        assert_eq!(ch.len(), 3);
        assert_eq!(ch[1].0, Duration::from_secs(20));
        assert_eq!(ch[2].1, "Ünïcode Ending");
    }
}

#[cfg(test)]
mod live {
    /// Network: `cargo test --release spoken_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn spoken_live() {
        let base = std::env::temp_dir().join(format!("bman-spoken-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let s = super::Spoken::new(base.clone());
        let wait = |s: &super::Spoken| {
            while s.busy() {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        };
        s.search_podcasts("radiolab".into());
        wait(&s);
        let pods = s.podcast_results();
        println!("podcasts: {} ({}), first: {:?}", pods.len(), s.status(), pods.first().map(|p| (&p.title, &p.feed)));
        s.search_books("pride and prejudice".into());
        wait(&s);
        let books = s.book_results();
        println!("books: {} ({}), first: {:?}", books.len(), s.status(), books.first().map(|b| (b.label(), b.chapters.first())));
        if let Some(p) = pods.first() {
            s.subscribe(&p.feed);
            wait(&s);
            println!("subscribe: {}", s.status());
            let shows = super::scan_shows(&base.join(super::PODCASTS_DIR));
            println!("shows: {:?}", shows.iter().map(|s| (&s.title, s.episodes.len())).collect::<Vec<_>>());
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
