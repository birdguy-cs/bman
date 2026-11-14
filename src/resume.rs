//! Carrying playback across a restart of bman (after an update, or
//! Settings > System > Restart bman): the queue, which song to play and from
//! where, paused or not, and the loop mode. Written to `.bman-resume` in
//! bman's folder just before restarting, read and deleted by the new
//! process. A file older than RESUME_MAX_AGE is ignored, so an old one (e.g.
//! left by a crash) never starts music at some later boot.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

const FILE: &str = ".bman-resume";
const RESUME_MAX_AGE: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Default, PartialEq)]
pub struct Resume {
    /// Why bman restarted ("update" or "restart").
    pub reason: String,
    pub songs: Vec<PathBuf>,
    /// Song to play; None = just restore the queue.
    pub index: Option<usize>,
    pub position_ms: u64,
    pub paused: bool,
    /// LoopMode label: "Off", "Track", "All".
    pub loop_mode: String,
}

impl Resume {
    fn to_text(&self) -> String {
        let mut text = format!(
            "reason={}\nindex={}\nposition_ms={}\npaused={}\nloop={}\n",
            self.reason,
            self.index.map_or("none".to_string(), |i| i.to_string()),
            self.position_ms,
            self.paused,
            self.loop_mode,
        );
        for song in &self.songs {
            text.push_str(&format!("song={}\n", song.display()));
        }
        text
    }

    fn from_text(text: &str) -> Self {
        let mut resume = Resume::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            match key {
                "reason" => resume.reason = value.to_string(),
                "index" => resume.index = value.parse().ok(),
                "position_ms" => resume.position_ms = value.parse().unwrap_or(0),
                "paused" => resume.paused = value == "true",
                "loop" => resume.loop_mode = value.to_string(),
                "song" => resume.songs.push(PathBuf::from(value)),
                _ => {}
            }
        }
        resume
    }
}

pub fn save(resume: &Resume) {
    if let Err(e) = fs::write(FILE, resume.to_text()) {
        warn!("Couldn't save the queue for after the restart: {e}");
    }
}

/// The saved state, if there's a recent one. Deletes the file either way.
pub fn take() -> Option<Resume> {
    let age = fs::metadata(FILE).ok()?.modified().ok()?.elapsed().unwrap_or_default();
    let text = fs::read_to_string(FILE).ok();
    let _ = fs::remove_file(FILE);
    if age > RESUME_MAX_AGE {
        return None;
    }
    Some(Resume::from_text(&text?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let resume = Resume {
            reason: "update".into(),
            songs: vec!["music/A - B.flac".into(), "music/x=y/C.mp3".into()],
            index: Some(1),
            position_ms: 61_500,
            paused: true,
            loop_mode: "All".into(),
        };
        assert_eq!(Resume::from_text(&resume.to_text()), resume);
        let none = Resume { index: None, ..Default::default() };
        assert_eq!(Resume::from_text(&none.to_text()).index, None);
    }
}
