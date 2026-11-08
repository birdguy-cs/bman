use image::DynamicImage;
use lofty::file::TaggedFileExt;
use lofty::picture::PictureType;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
};

/// Cover art is shrunk to this many pixels square. Keeping it small keeps
/// memory and drawing cost low on a Pi Zero 2 W.
pub const ART_SIZE: u32 = 96;

/// Folder images named "cover.<ext>" are used when a song has no embedded art.
const COVER_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "bmp"];

pub struct Art {
    pub size: u32,
    /// size * size pixels, 0xFFRRGGBB.
    pub pixels: Vec<u32>,
}

/// Load art for `song_path` on a background thread (decoding a big JPEG
/// would freeze the UI). Embedded art (ID3 in MP3/AIFF, FLAC pictures, ...)
/// wins; otherwise a `cover.jpg` / `cover.png` in the song's folder is used.
/// The receiver gets `None` if neither exists.
pub fn spawn_art_loader(song_path: PathBuf) -> Receiver<Option<Art>> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        crate::sched::background(); // never compete with playback
        let art = embedded_art(&song_path).or_else(|| folder_art(&song_path));
        let _ = tx.send(art); // receiver is gone if the track already changed
    });

    rx
}

fn embedded_art(song: &Path) -> Option<Art> {
    let tagged = lofty::read_from_path(song).ok()?;

    let pictures: Vec<_> = tagged.tags().iter().flat_map(|t| t.pictures()).collect();
    let picture = pictures
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.first())?;

    let image = image::load_from_memory(picture.data()).ok()?;
    to_art(image)
}

fn folder_art(song: &Path) -> Option<Art> {
    let dir = song.parent()?;

    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let (Some(stem), Some(ext)) = (path.file_stem(), path.extension()) else {
            continue;
        };

        let stem = stem.to_string_lossy().to_lowercase();
        let ext = ext.to_string_lossy().to_lowercase();

        if stem == "cover" && COVER_EXTENSIONS.contains(&ext.as_str()) {
            if let Ok(image) = image::open(&path) {
                return to_art(image);
            }
        }
    }

    None
}

/// Center-crop to a square, shrink, and convert to 0xFFRRGGBB pixels.
fn to_art(image: DynamicImage) -> Option<Art> {
    let (w, h) = (image.width(), image.height());
    if w == 0 || h == 0 {
        return None;
    }

    let side = w.min(h);
    let square = image.crop_imm((w - side) / 2, (h - side) / 2, side, side);
    let small = square.thumbnail_exact(ART_SIZE, ART_SIZE).to_rgb8();

    let pixels = small
        .pixels()
        .map(|p| 0xFF000000 | (p.0[0] as u32) << 16 | (p.0[1] as u32) << 8 | p.0[2] as u32)
        .collect();

    Some(Art {
        size: ART_SIZE,
        pixels,
    })
}
