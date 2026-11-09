use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fs, io,
    iter::Peekable,
    path::{Path, PathBuf},
    str::Chars,
};

/// File types rodio can decode. AIFF needs symphonia's "aiff" feature, see Cargo.toml.
const EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "ogg", "aiff", "aif"];
/// Folder for videos, next to the music folder.
pub const VIDEOS_DIR: &str = "videos";
const PLAYLIST_EXTENSIONS: &[&str] = &["m3u", "m3u8"];

#[derive(Debug, Clone)]
pub struct Song {
    /// Just the name: "Veiled Orbits".
    pub title: String,
    /// Track number from the filename, if it has one.
    pub track: Option<u32>,
    pub path: PathBuf,
    pub artist: Option<String>,
    pub album: String,
}

impl Song {
    /// "Artist - Title". Used where songs from different albums are mixed.
    pub fn label(&self) -> String {
        match &self.artist {
            Some(artist) => format!("{artist} - {}", self.title),
            None => self.title.clone(),
        }
    }

    /// "02 Title". Used inside a single album.
    pub fn album_label(&self) -> String {
        match self.track {
            Some(n) => format!("{n:02} {}", self.title),
            None => self.title.clone(),
        }
    }
}

/// One folder (below the music root) that directly contains songs.
#[derive(Debug, Clone)]
pub struct Album {
    pub name: String,
    pub artist: Option<String>,
    /// In album order (by track number, then name).
    pub songs: Vec<Song>,
}

impl Album {
    /// "Artist - Album", used in the album list.
    pub fn label(&self) -> String {
        match &self.artist {
            Some(artist) => format!("{artist} - {}", self.name),
            None => self.name.clone(),
        }
    }
}

/// Everything by one artist: albums first, then singles.
/// The numbers are indexes into `Library::albums` / `Library::singles`.
#[derive(Debug, Clone)]
pub struct Artist {
    pub name: String,
    pub albums: Vec<usize>,
    pub singles: Vec<usize>,
}

/// An `.m3u` file. Changes are written straight back to the file.
#[derive(Debug, Clone)]
pub struct Playlist {
    pub name: String,
    pub path: PathBuf,
    pub songs: Vec<Song>,
}

#[derive(Debug, Default)]
pub struct Library {
    pub root: PathBuf,
    /// Songs sitting directly in the music folder, sorted alphabetically.
    pub singles: Vec<Song>,
    /// Every subfolder containing songs, sorted alphabetically.
    pub albums: Vec<Album>,
    /// Everyone with albums or singles, sorted alphabetically.
    /// Anything without a known artist is grouped last as "Unknown Artist".
    pub artists: Vec<Artist>,
    /// Every .m3u / .m3u8 found in the music folder, sorted by name.
    pub playlists: Vec<Playlist>,
    /// Every video in the `videos` folder next to the music folder (and its
    /// subfolders), named like singles, sorted alphabetically.
    pub videos: Vec<Song>,
    /// Problems found while scanning (unreadable folders, missing playlist
    /// entries). Collected instead of printed so rescans don't spam the console.
    pub warnings: Vec<String>,
}

thread_local! {
    static WARNINGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn warn(message: String) {
    WARNINGS.with(|w| w.borrow_mut().push(message));
}

impl Library {
    /// Recursively scan `root` (symlinks are followed). Songs directly in
    /// `root` are singles ("Artist - Song.mp3"); every subfolder that directly
    /// contains songs is an album ("Artist - Album/Artist - Album - 01 Song.mp3");
    /// `.m3u` files are playlists.
    ///
    /// Only fails if `root` itself can't be read. Unreadable subfolders are
    /// skipped with a warning.
    pub fn scan(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();

        // metadata() follows links, so this checks what the path really is.
        if !fs::metadata(root)?.is_dir() {
            let real = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a file, not a folder", real.display()),
            ));
        }

        WARNINGS.with(|w| w.borrow_mut().clear());
        let mut singles = Vec::new();
        let mut albums = Vec::new();
        let mut playlist_files = Vec::new();
        let mut visited = HashSet::new();
        scan_dir(
            root,
            root,
            &mut singles,
            &mut albums,
            &mut playlist_files,
            &mut visited,
        );

        albums.sort_by(|a, b| natural_cmp(&a.label(), &b.label()));

        let mut playlists = load_playlists(&playlist_files, &singles, &albums);
        playlists.sort_by(|a, b| natural_cmp(&a.name, &b.name));

        let artists = build_artists(&singles, &albums);

        // `videos` beside the music folder (beside the link, if it's one).
        let mut videos = Vec::new();
        let videos_dir = root.with_file_name(VIDEOS_DIR);
        if videos_dir.is_dir() {
            scan_videos(&videos_dir, &mut videos, &mut visited);
            videos.sort_by(|a, b| natural_cmp(&a.label(), &b.label()));
        }

        Ok(Self {
            root: root.to_path_buf(),
            singles,
            albums,
            artists,
            playlists,
            videos,
            warnings: WARNINGS.with(|w| std::mem::take(&mut *w.borrow_mut())),
        })
    }

    /// The library's song for a file path (singles, albums, playlists, videos).
    pub fn song_by_path(&self, path: &Path) -> Option<Song> {
        self.singles
            .iter()
            .chain(self.albums.iter().flat_map(|a| &a.songs))
            .chain(self.playlists.iter().flat_map(|p| &p.songs))
            .chain(&self.videos)
            .find(|s| s.path == path)
            .cloned()
    }

    pub fn song_count(&self) -> usize {
        self.singles.len() + self.albums.iter().map(|a| a.songs.len()).sum::<usize>()
    }

    /// Create an empty "Playlist N.m3u" in the music folder.
    /// Returns its index. The app then offers to name it (`rename_playlist`).
    pub fn create_playlist(&mut self) -> io::Result<usize> {
        self.create_playlist_with(Vec::new())
    }

    /// Like `create_playlist`, but filled with `songs` (e.g. the queue).
    pub fn create_playlist_with(&mut self, songs: Vec<Song>) -> io::Result<usize> {
        let mut n = self.playlists.len() + 1;

        loop {
            let name = format!("Playlist {n}");
            let path = self.root.join(format!("{name}.m3u"));

            if !path.exists() {
                let playlist = Playlist {
                    name,
                    path,
                    songs,
                };
                playlist.save()?;
                self.playlists.push(playlist);
                return Ok(self.playlists.len() - 1);
            }

            n += 1;
        }
    }

    /// Rename a playlist's .m3u file (keeps its folder and extension).
    /// Characters that are invalid in file names on common file systems
    /// (FAT/exFAT SD cards, Windows shares) are refused.
    pub fn rename_playlist(&mut self, index: usize, new_name: &str) -> Result<(), String> {
        const BAD: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return Err("Name is empty".into());
        }
        if let Some(c) = new_name.chars().find(|c| BAD.contains(c)) {
            return Err(format!("Can't use {c} in names"));
        }

        let playlist = self.playlists.get(index).ok_or("Playlist gone")?;
        let ext = playlist
            .path
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_else(|| "m3u".into());
        let new_path = playlist
            .path
            .with_file_name(format!("{new_name}.{ext}"));

        if new_path == playlist.path {
            return Ok(());
        }
        // A case-only rename is the same file on case-insensitive systems.
        let same_file = new_path.to_string_lossy().to_lowercase()
            == playlist.path.to_string_lossy().to_lowercase();
        if new_path.exists() && !same_file {
            return Err("Name already used".into());
        }

        fs::rename(&playlist.path, &new_path).map_err(|e| {
            error!("Couldn't rename playlist: {e}");
            "Couldn't rename".to_string()
        })?;

        let playlist = &mut self.playlists[index];
        playlist.path = new_path;
        playlist.name = new_name.to_string();
        Ok(())
    }

    /// Returns Ok(false) if the song was already in the playlist.
    pub fn add_to_playlist(&mut self, index: usize, song: Song) -> io::Result<bool> {
        let Some(playlist) = self.playlists.get_mut(index) else {
            return Ok(false);
        };

        if playlist.songs.iter().any(|s| s.path == song.path) {
            return Ok(false);
        }

        playlist.songs.push(song);
        playlist.save()?;
        Ok(true)
    }

    pub fn remove_from_playlist(&mut self, index: usize, song_index: usize) -> io::Result<()> {
        let Some(playlist) = self.playlists.get_mut(index) else {
            return Ok(());
        };

        if song_index < playlist.songs.len() {
            playlist.songs.remove(song_index);
            playlist.save()?;
        }
        Ok(())
    }
}

impl Playlist {
    /// Write the .m3u. Paths inside the music folder are stored relative to
    /// the playlist so the whole folder can be moved. Written to a temp file
    /// first so pulling the plug can't corrupt the playlist.
    pub fn save(&self) -> io::Result<()> {
        let dir = self.path.parent().unwrap_or(Path::new(""));

        let mut out = String::from("#EXTM3U\n");
        for song in &self.songs {
            let path = song.path.strip_prefix(dir).unwrap_or(&song.path);
            out.push_str(&path.to_string_lossy().replace('\\', "/"));
            out.push('\n');
        }

        let tmp = self.path.with_extension("m3u.tmp");
        fs::write(&tmp, out)?;
        fs::rename(&tmp, &self.path)
    }
}

fn scan_dir(
    dir: &Path,
    root: &Path,
    singles: &mut Vec<Song>,
    albums: &mut Vec<Album>,
    playlist_files: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
) {
    // Symlinks are followed, so remember every folder (by its real location)
    // to avoid looping forever or listing the same folder twice.
    let real = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !visited.insert(real) {
        warn(format!("Skipping {} (already scanned, symlink loop?)", dir.display()));
        return;
    }

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            warn(format!("Skipping {}: {e}", dir.display()));
            return;
        }
    };

    let is_root = dir == root;
    let folder = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let (artist, album) = if is_root {
        (None, "Singles".to_string())
    } else {
        folder_artist_album(dir, root, &folder)
    };

    let mut songs = Vec::new();
    let mut subdirs = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // hidden files and folders
        }

        // metadata() follows symlinks (entry.file_type() does not), so linked
        // folders and files work. Broken links are skipped.
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };

        if meta.is_dir() {
            subdirs.push(path);
        } else if meta.is_file() && has_extension(&path, PLAYLIST_EXTENSIONS) {
            playlist_files.push(path);
        } else if meta.is_file() && is_playable(&path) {
            songs.push(parse_song(path, artist.clone(), &album, !is_root, &folder));
        }
    }

    // Natural sort: "2 Song" comes before "10 Song".
    if is_root {
        songs.sort_by(|a, b| natural_cmp(&a.label(), &b.label()));
    } else {
        songs.sort_by(|a, b| {
            a.track
                .cmp(&b.track)
                .then_with(|| natural_cmp(&a.title, &b.title))
        });
    }
    subdirs.sort();

    if is_root {
        singles.extend(songs);
    } else if !songs.is_empty() {
        albums.push(Album {
            name: album,
            artist,
            songs,
        });
    }

    for sub in subdirs {
        scan_dir(&sub, root, singles, albums, playlist_files, visited);
    }
}

/// Every video under `dir`, as singles ("Artist - Title.mp4").
fn scan_videos(dir: &Path, videos: &mut Vec<Song>, visited: &mut HashSet<PathBuf>) {
    let real = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !visited.insert(real) {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            warn(format!("Skipping {}: {e}", dir.display()));
            return;
        }
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else { continue };
        if meta.is_dir() {
            scan_videos(&path, videos, visited);
        } else if meta.is_file() && crate::video::is_video_file(&path) {
            videos.push(parse_song(path, None, "Videos", false, ""));
        }
    }
}

/// Work out artist and album from a folder.
///   music/Artist - Album/...   -> ("Artist", "Album")
///   music/Artist/Album/...     -> ("Artist", "Album")
///   music/Album/...            -> (None, "Album")
fn folder_artist_album(dir: &Path, root: &Path, folder: &str) -> (Option<String>, String) {
    if let Some(parent) = dir.parent() {
        if parent != root {
            if let Some(name) = parent.file_name() {
                return (Some(name.to_string_lossy().to_string()), folder.to_string());
            }
        }
    }

    match split_artist_title(folder) {
        Some((artist, album)) => (Some(artist), album),
        None => (None, clean_name(folder)),
    }
}

/// Build a Song from a filename.
///   album song: "Artist - Album - 02 Title.aiff" -> track 2, title "Title"
///   single:     "Artist - Title.mp3" or "Title by Artist.mp3" (see
///               `split_artist_title` for every form)
fn parse_song(path: PathBuf, artist: Option<String>, album: &str, in_album: bool, folder: &str) -> Song {
    let stem = clean_name(&title_from(&path));
    let folder = clean_name(folder);

    if in_album {
        let (track, title) = split_track(&stem, &folder);
        return Song {
            title,
            track,
            path,
            artist,
            album: album.to_string(),
        };
    }

    match split_artist_title(&stem) {
        Some((artist, title)) => Song {
            title,
            track: None,
            path,
            artist: Some(artist),
            album: album.to_string(),
        },
        _ => Song {
            title: stem.clone(),
            track: None,
            path,
            artist,
            album: album.to_string(),
        },
    }
}

/// "Artist_-_Song_Title" (no spaces at all) -> "Artist - Song Title".
fn clean_name(name: &str) -> String {
    if !name.contains(' ') && name.contains('_') {
        name.replace('_', " ").trim().to_string()
    } else {
        name.trim().to_string()
    }
}

/// Words that can come before "by" and belong to it, not the title:
/// "Song made by Artist", "Song performed by Artist", ...
const BY_VERBS: &[&str] = &[
    "made", "performed", "sung", "played", "recorded", "produced", "written",
    "composed", "covered", "remixed", "mixed", "arranged",
];

/// Right-hand words that mean " by " is part of the title, not a credit:
/// "Stand by Me", "Stand by Your Man".
const BY_NOT_ARTIST: &[&str] = &["me", "you", "your", "my", "us", "him", "her", "them", "it", "myself", "yourself"];

/// Find artist and title in a name (file stem or folder). Returns
/// (artist, title). Accepts, in this order:
///   "Artist - Title"   (also with – or — dashes)
///   "Title - by Artist"
///   "Title (by Artist)" / "Title [by Artist]" / "Title (made by Artist)"
///   "Title by Artist", "Title, by Artist", "Title made by Artist",
///   "Title performed by Artist", ... (see BY_VERBS)
/// "by" matches any upper/lower case. The last "by" wins, so
/// "Stand by Your Man by Tammy Wynette" works, and titles like
/// "Stand by Me" or "Day by Day" aren't split.
fn split_artist_title(name: &str) -> Option<(String, String)> {
    let name = name.trim();
    let ok = |artist: &str, title: &str| {
        let (artist, title) = (artist.trim(), title.trim());
        (!artist.is_empty() && !title.is_empty()).then(|| (artist.to_string(), title.to_string()))
    };

    // Dash forms.
    for sep in [" - ", " \u{2013} ", " \u{2014} "] {
        if let Some((left, right)) = name.split_once(sep) {
            if let Some(artist) = strip_by_prefix(right) {
                return ok(artist, left); // "Title - by Artist"
            }
            return ok(left, right);
        }
    }

    // "Title (by Artist)" / "Title [made by Artist]"
    for (open, close) in [('(', ')'), ('[', ']')] {
        if name.ends_with(close) {
            if let Some(start) = name.rfind(open) {
                let inner = &name[start + 1..name.len() - 1];
                if let Some(artist) = strip_by_prefix(inner) {
                    return ok(artist, &name[..start]);
                }
            }
        }
    }

    // "Title by Artist", "Title made by Artist", ...
    let lower = name.to_ascii_lowercase();
    let pos = lower.rfind(" by ")?;
    let title = &name[..pos];
    let artist = &name[pos + 4..];

    let first_word = artist.split_whitespace().next()?.to_ascii_lowercase();
    let last_word = title.split_whitespace().last()?.to_ascii_lowercase();
    if BY_NOT_ARTIST.contains(&first_word.as_str()) || first_word == last_word {
        return None; // "Stand by Me", "Little by Little"
    }

    // Drop a credit verb or comma before "by".
    let mut title = title.trim_end();
    if let Some((rest, word)) = title.rsplit_once(' ') {
        if BY_VERBS.contains(&word.to_ascii_lowercase().as_str()) {
            title = rest;
        }
    }
    let title = title.trim_end_matches([',', ' ', '-']);
    ok(artist, title)
}

/// "by Artist" / "made by Artist" -> "Artist".
fn strip_by_prefix(text: &str) -> Option<&str> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("by ") {
        return Some(&text[3..]);
    }
    let (verb, _) = lower.split_once(' ')?;
    if BY_VERBS.contains(&verb) && lower[verb.len()..].trim_start().starts_with("by ") {
        let after = text[verb.len()..].trim_start();
        return Some(&after[3..]);
    }
    None
}

/// Split "Artist - Album - 02 Title" into (Some(2), "Title").
fn split_track(stem: &str, folder: &str) -> (Option<u32>, String) {
    // Usually the filename starts with the folder name: "<folder> - 02 Title".
    let prefix = folder.len();
    if !folder.is_empty()
        && stem.len() > prefix + 3
        && stem.is_char_boundary(prefix)
        && stem[..prefix].eq_ignore_ascii_case(folder)
        && stem[prefix..].starts_with(" - ")
    {
        if let Some(found) = parse_track(&stem[prefix + 3..]) {
            return found;
        }
    }

    // Otherwise use the first " - " that is followed by a track number.
    for (i, _) in stem.match_indices(" - ") {
        if let Some(found) = parse_track(&stem[i + 3..]) {
            return found;
        }
    }

    (None, stem.to_string())
}

/// "02 Title", "02. Title" or "02 - Title" -> (Some(2), "Title").
fn parse_track(rest: &str) -> Option<(Option<u32>, String)> {
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 3 {
        return None;
    }

    let after = &rest[digits..];
    if !(after.starts_with(' ') || after.starts_with('.') || after.starts_with('-')) {
        return None;
    }

    let title = after
        .trim_start_matches(|c: char| c == '.' || c == ' ' || c == '-' || c == '_')
        .trim();
    if title.is_empty() {
        return None;
    }

    Some((rest[..digits].parse().ok(), title.to_string()))
}

/// Group albums and singles by artist (ignoring upper/lower case).
/// `albums` and `singles` are already sorted, so each artist's lists are too.
fn build_artists(singles: &[Song], albums: &[Album]) -> Vec<Artist> {
    fn entry<'a>(map: &'a mut HashMap<String, Artist>, name: Option<&str>) -> &'a mut Artist {
        let (key, display) = match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => (n.to_lowercase(), n.to_string()),
            None => (String::new(), "Unknown Artist".to_string()),
        };

        map.entry(key).or_insert_with(|| Artist {
            name: display,
            albums: Vec::new(),
            singles: Vec::new(),
        })
    }

    let mut map: HashMap<String, Artist> = HashMap::new();

    for (i, album) in albums.iter().enumerate() {
        entry(&mut map, album.artist.as_deref()).albums.push(i);
    }
    for (i, song) in singles.iter().enumerate() {
        entry(&mut map, song.artist.as_deref()).singles.push(i);
    }

    let mut artists: Vec<(String, Artist)> = map.into_iter().collect();
    artists.sort_by(|(ka, a), (kb, b)| match (ka.is_empty(), kb.is_empty()) {
        (true, false) => Ordering::Greater, // "Unknown Artist" goes last
        (false, true) => Ordering::Less,
        _ => natural_cmp(&a.name, &b.name),
    });

    artists.into_iter().map(|(_, artist)| artist).collect()
}

fn load_playlists(files: &[PathBuf], singles: &[Song], albums: &[Album]) -> Vec<Playlist> {
    if files.is_empty() {
        return Vec::new();
    }

    // Look songs up by canonical path so "a/../b.mp3" still matches "b.mp3".
    let mut lookup: HashMap<PathBuf, Song> = HashMap::new();
    for song in singles.iter().chain(albums.iter().flat_map(|a| a.songs.iter())) {
        let key = fs::canonicalize(&song.path).unwrap_or_else(|_| song.path.clone());
        lookup.insert(key, song.clone());
    }

    files
        .iter()
        .map(|path| Playlist {
            name: title_from(path),
            path: path.clone(),
            songs: parse_m3u(path, &lookup),
        })
        .collect()
}

/// Read an .m3u: one path per line, "#" lines are comments (#EXTM3U, #EXTINF).
/// Relative paths are relative to the playlist. Missing files are skipped.
fn parse_m3u(path: &Path, lookup: &HashMap<PathBuf, Song>) -> Vec<Song> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            warn(format!("Couldn't read playlist {}: {e}", path.display()));
            return Vec::new();
        }
    };

    let text = String::from_utf8_lossy(&bytes);
    let dir = path.parent().unwrap_or(Path::new(""));
    let mut songs = Vec::new();

    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let entry = PathBuf::from(line.replace('\\', "/"));
        let full = if entry.is_absolute() {
            entry
        } else {
            dir.join(entry)
        };

        let key = fs::canonicalize(&full).unwrap_or_else(|_| full.clone());
        match lookup.get(&key) {
            Some(song) => songs.push(song.clone()),
            None if full.is_file() && is_playable(&full) => {
                songs.push(parse_song(full, None, "Unknown", false, ""))
            }
            None => warn(format!("Playlist {}: skipping {}", path.display(), full.display())),
        }
    }

    songs
}

fn has_extension(path: &Path, list: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| list.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn is_playable(path: &Path) -> bool {
    has_extension(path, EXTENSIONS)
}

fn title_from(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

/// Case-insensitive comparison where digit runs compare as numbers.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut a = a.chars().peekable();
    let mut b = b.chars().peekable();

    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let nx = take_number(&mut a);
                let ny = take_number(&mut b);
                match nx.cmp(&ny) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
            (Some(x), Some(y)) => {
                match x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase()) {
                    Ordering::Equal => {
                        a.next();
                        b.next();
                    }
                    other => return other,
                }
            }
        }
    }
}

fn take_number(it: &mut Peekable<Chars<'_>>) -> u64 {
    let mut n: u64 = 0;
    while let Some(c) = it.peek().copied() {
        match c.to_digit(10) {
            Some(d) => {
                n = n.saturating_mul(10).saturating_add(d as u64);
                it.next();
            }
            None => break,
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(name: &str) -> Option<(String, String)> {
        split_artist_title(name)
    }
    fn pair(artist: &str, title: &str) -> Option<(String, String)> {
        Some((artist.to_string(), title.to_string()))
    }

    #[test]
    fn naming_forms() {
        assert_eq!(split("Daft Punk - One More Time"), pair("Daft Punk", "One More Time"));
        assert_eq!(split("Daft Punk \u{2013} One More Time"), pair("Daft Punk", "One More Time"));
        assert_eq!(split("One More Time - by Daft Punk"), pair("Daft Punk", "One More Time"));
        assert_eq!(split("One More Time by Daft Punk"), pair("Daft Punk", "One More Time"));
        assert_eq!(split("One More Time BY Daft Punk"), pair("Daft Punk", "One More Time"));
        assert_eq!(split("One More Time, by Daft Punk"), pair("Daft Punk", "One More Time"));
        assert_eq!(split("One More Time made by Daft Punk"), pair("Daft Punk", "One More Time"));
        assert_eq!(split("One More Time performed by Daft Punk"), pair("Daft Punk", "One More Time"));
        assert_eq!(split("One More Time (by Daft Punk)"), pair("Daft Punk", "One More Time"));
        assert_eq!(split("One More Time [made by Daft Punk]"), pair("Daft Punk", "One More Time"));
        assert_eq!(split("Stand by Your Man by Tammy Wynette"), pair("Tammy Wynette", "Stand by Your Man"));
        assert_eq!(split("Stand by Me"), None);
        assert_eq!(split("Little by Little"), None);
        assert_eq!(split("Just A Title"), None);
        assert_eq!(clean_name("Daft_Punk_-_One_More_Time"), "Daft Punk - One More Time");
        assert_eq!(clean_name("Song Name_v2"), "Song Name_v2");
    }
}
