//! Videos on Now Playing: YouTube's 144p H.264 video track (fragmented
//! MP4, as yt-dlp downloads it without ffmpeg) or a local .mp4/.m4v/.mov
//! from the videos folder (H.264), decoded with OpenH264 on a background
//! thread and shown in sync with the audio.
//!
//! The decoder runs a few frames ahead in a small channel; the screen
//! takes whichever frame is due at the audio position. Late frames are
//! decoded (H.264 needs them as references) but not converted, so a slow
//! Pi skips frames instead of falling behind. Jumping in the song (restart,
//! resume) reopens the video at the keyframe before that point.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::Duration;

use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;

/// Size of the video area on Now Playing (16:9 across the screen).
pub const VIDEO_W: usize = 240;
pub const VIDEO_H: usize = 135;
/// Frames decoded ahead of the one on screen.
const AHEAD: usize = 3;
/// Further than this from what's on screen counts as a jump: reopen there.
const JUMP: Duration = Duration::from_secs(3);

/// Local video files (the videos folder): MP4 / QuickTime with H.264.
pub const EXTENSIONS: &[&str] = &["mp4", "m4v", "mov"];

/// A local video file (by extension).
pub fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Length from the file's movie header. rodio works it out wrong for files
/// with a video track (it uses the video track's timescale).
pub fn duration(path: &Path) -> Option<Duration> {
    let mut file = std::fs::File::open(path).ok()?;
    mp4::duration(&mut file)
}

pub struct Frame {
    pub pts: Duration,
    /// VIDEO_W x VIDEO_H, 0xFFRRGGBB.
    pub pixels: Vec<u32>,
}

pub struct Video {
    path: PathBuf,
    rx: Receiver<Frame>,
    stop: Arc<AtomicBool>,
    /// Audio position in ms, so the decoder knows which frames are late.
    position: Arc<AtomicU64>,
    shown: Option<Frame>,
    next: Option<Frame>,
    /// Where it was opened (the first frame can be a bit before this).
    start: Duration,
    finished: bool,
}

impl Video {
    /// Start decoding `path` from `at`.
    pub fn open(path: &Path, at: Duration) -> Self {
        let (tx, rx) = mpsc::sync_channel(AHEAD);
        let stop = Arc::new(AtomicBool::new(false));
        let position = Arc::new(AtomicU64::new(at.as_millis() as u64));
        let (p, s, pos) = (path.to_path_buf(), stop.clone(), position.clone());
        thread::Builder::new()
            .name("bman-video".into())
            .spawn(move || {
                crate::sched::background();
                if let Err(e) = decode(&p, at, &tx, &s, &pos) {
                    warn!("Video {}: {e}", p.display());
                }
            })
            .ok();
        Self {
            path: path.to_path_buf(),
            rx,
            stop,
            position,
            shown: None,
            next: None,
            start: at,
            finished: false,
        }
    }

    /// The frame to show at audio position `pos` (None until the first is ready).
    pub fn frame(&mut self, pos: Duration) -> Option<&[u32]> {
        self.position.store(pos.as_millis() as u64, Ordering::Relaxed);

        // Jumped back, or far ahead of what's coming: start over from there.
        let at = self.next.as_ref().or(self.shown.as_ref()).map(|f| f.pts).unwrap_or(self.start);
        let back = self.shown.as_ref().is_some_and(|f| f.pts > pos + JUMP) || pos + JUMP < self.start;
        let ahead = !self.finished && pos > at + JUMP && (self.shown.is_some() || pos > self.start + JUMP);
        if back || ahead {
            *self = Video::open(&self.path.clone(), pos);
            return None;
        }

        loop {
            if self.next.is_none() {
                match self.rx.try_recv() {
                    Ok(frame) => self.next = Some(frame),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.finished = true;
                        break;
                    }
                }
            }
            match &self.next {
                Some(f) if f.pts <= pos => self.shown = self.next.take(),
                _ => break,
            }
        }
        self.shown.as_ref().map(|f| f.pixels.as_slice())
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn decode(
    path: &Path,
    at: Duration,
    tx: &SyncSender<Frame>,
    stop: &AtomicBool,
    position: &AtomicU64,
) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let track = mp4::parse(&mut file)?;
    let scale = track.timescale.max(1) as f64;
    let to_time = |pts: i64| Duration::from_secs_f64(pts.max(0) as f64 / scale);

    // Start at the last keyframe at or before `at`.
    let target = (at.as_secs_f64() * scale) as i64;
    let first = track
        .samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.keyframe && s.pts <= target)
        .map(|(i, _)| i)
        .next_back()
        .unwrap_or(0);
    let samples = &track.samples[first..];
    // The decoder hands frames back in display order: the n-th one out has
    // the n-th smallest presentation time.
    let mut times: Vec<i64> = samples.iter().map(|s| s.pts).collect();
    times.sort_unstable();
    let mut times = times.into_iter();

    let config = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
    let mut decoder =
        Decoder::with_api_config(OpenH264API::from_source(), config).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    for p in &track.params {
        buf.extend_from_slice(&[0, 0, 0, 1]);
        buf.extend_from_slice(p);
    }
    let _ = decoder.decode(&buf);

    // Convert and send one decoded picture; false = stop.
    let emit = |yuv: &dyn YUVSource, pts: Option<i64>| -> bool {
        let Some(pts) = pts else { return true };
        let time = to_time(pts);
        let now = Duration::from_millis(position.load(Ordering::Relaxed));
        if time + Duration::from_millis(100) < now {
            return true; // late: skip the conversion, keep decoding
        }
        let frame = Frame { pts: time, pixels: to_rgb(yuv) };
        !stop.load(Ordering::Relaxed) && tx.send(frame).is_ok()
    };

    let mut errors = 0;
    let mut data = Vec::new();
    for s in samples {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        data.resize(s.size as usize, 0);
        if file.seek(SeekFrom::Start(s.offset)).and_then(|_| file.read_exact(&mut data)).is_err() {
            break;
        }
        buf.clear();
        mp4::annex_b(&data, track.nal_len, &mut buf);
        match decoder.decode(&buf) {
            Ok(Some(yuv)) => {
                if !emit(&yuv, times.next()) {
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(e) => {
                errors += 1;
                if errors == 1 {
                    warn!("Video decode error: {e}");
                }
            }
        }
    }
    if let Ok(rest) = decoder.flush_remaining() {
        for yuv in rest {
            if !emit(&yuv, times.next()) {
                break;
            }
        }
    }
    Ok(())
}

/// YUV 4:2:0 (BT.601, limited range) to VIDEO_W x VIDEO_H RGB, scaled to
/// fit (nearest pixel) and letterboxed in black.
fn to_rgb(yuv: &dyn YUVSource) -> Vec<u32> {
    let (w, h) = yuv.dimensions();
    let (ys, us, vs) = yuv.strides();
    let (y_plane, u_plane, v_plane) = (yuv.y(), yuv.u(), yuv.v());
    let mut out = vec![0xFF00_0000u32; VIDEO_W * VIDEO_H];
    if w == 0 || h == 0 {
        return out;
    }
    // Fit inside the area, keeping the shape.
    let (dw, dh) = if w * VIDEO_H > h * VIDEO_W {
        (VIDEO_W, (h * VIDEO_W / w).max(1))
    } else {
        ((w * VIDEO_H / h).max(1), VIDEO_H)
    };
    let (ox, oy) = ((VIDEO_W - dw) / 2, (VIDEO_H - dh) / 2);
    for dy in 0..dh {
        let sy = dy * h / dh;
        let row = &mut out[(oy + dy) * VIDEO_W + ox..][..dw];
        for (dx, px) in row.iter_mut().enumerate() {
            let sx = dx * w / dw;
            let c = *y_plane.get(sy * ys + sx).unwrap_or(&16) as i32 - 16;
            let d = *u_plane.get((sy / 2) * us + sx / 2).unwrap_or(&128) as i32 - 128;
            let e = *v_plane.get((sy / 2) * vs + sx / 2).unwrap_or(&128) as i32 - 128;
            let r = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u32;
            let g = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u32;
            let b = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u32;
            *px = 0xFF00_0000 | r << 16 | g << 8 | b;
        }
    }
    out
}

/// Just enough of MP4 for H.264 video: the setup from `moov`, and every
/// frame's time, place and size, either from the `moov` sample tables
/// (ordinary .mp4/.mov) or from the `moof`s (fragmented, YouTube's DASH).
/// Only `moov` and the `moof`s are read into memory; frames are read from
/// the file as they're decoded.
mod mp4 {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};

    pub struct Sample {
        /// Presentation time in timescale units.
        pub pts: i64,
        pub offset: u64,
        pub size: u32,
        pub keyframe: bool,
    }

    pub struct Track {
        pub timescale: u32,
        /// Bytes in each NAL unit's length prefix.
        pub nal_len: usize,
        /// SPS and PPS NAL units.
        pub params: Vec<Vec<u8>>,
        /// In decode order.
        pub samples: Vec<Sample>,
    }

    const BAD: &str = "Not a video bman can play";

    fn be16(d: &[u8], i: usize) -> Option<u16> {
        Some(u16::from_be_bytes(d.get(i..i + 2)?.try_into().ok()?))
    }
    fn be32(d: &[u8], i: usize) -> Option<u32> {
        Some(u32::from_be_bytes(d.get(i..i + 4)?.try_into().ok()?))
    }
    fn be64(d: &[u8], i: usize) -> Option<u64> {
        Some(u64::from_be_bytes(d.get(i..i + 8)?.try_into().ok()?))
    }

    /// The boxes in `d`: (type, contents, where the box starts in `d`).
    fn boxes(d: &[u8]) -> impl Iterator<Item = (&[u8; 4], &[u8], usize)> {
        let mut i = 0;
        std::iter::from_fn(move || {
            let mut size = be32(d, i)? as u64;
            let kind: &[u8; 4] = d.get(i + 4..i + 8)?.try_into().ok()?;
            let mut header = 8;
            if size == 1 {
                size = be64(d, i + 8)?;
                header = 16;
            } else if size == 0 {
                size = (d.len() - i) as u64;
            }
            let end = i.checked_add(usize::try_from(size).ok()?)?;
            if size < header as u64 || end > d.len() {
                return None;
            }
            let item = (kind, &d[i + header..end], i);
            i = end;
            Some(item)
        })
    }

    fn child<'a>(d: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
        boxes(d).find(|(k, _, _)| *k == kind).map(|(_, b, _)| b)
    }

    /// A `moof` box and where in the file it starts.
    type Moof = (u64, Vec<u8>);

    /// The file's top-level `moov`, and each `moof` with where it starts.
    /// Everything else (the big `mdat`) is skipped over.
    fn read_top(file: &mut File) -> Result<(Vec<u8>, Vec<Moof>), String> {
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        let (mut moov, mut moofs) = (None, Vec::new());
        let mut pos = 0u64;
        let mut head = [0u8; 16];
        while pos + 8 <= len {
            file.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
            let n = ((len - pos).min(16)) as usize;
            file.read_exact(&mut head[..n]).map_err(|e| e.to_string())?;
            let mut size = be32(&head, 0).ok_or(BAD)? as u64;
            let mut header = 8u64;
            if size == 1 {
                size = be64(&head[..n], 8).ok_or(BAD)?;
                header = 16;
            } else if size == 0 {
                size = len - pos;
            }
            if size < header || pos + size > len {
                break; // cut-off file: use what's there
            }
            let kind = &head[4..8];
            if kind == b"moov" || kind == b"moof" {
                if size > 64 << 20 {
                    return Err(BAD.into());
                }
                let mut body = vec![0; (size - header) as usize];
                file.seek(SeekFrom::Start(pos + header)).map_err(|e| e.to_string())?;
                file.read_exact(&mut body).map_err(|e| e.to_string())?;
                if kind == b"moov" {
                    moov = Some(body);
                } else {
                    moofs.push((pos, body));
                }
            }
            pos += size;
        }
        Ok((moov.ok_or(BAD)?, moofs))
    }

    pub fn duration(file: &mut File) -> Option<std::time::Duration> {
        let (moov, _) = read_top(file).ok()?;
        let mvhd = child(&moov, b"mvhd")?;
        let (scale, dur) = if mvhd.first() == Some(&1) {
            (be32(mvhd, 20)?, be64(mvhd, 24)?)
        } else {
            (be32(mvhd, 12)?, be32(mvhd, 16)? as u64)
        };
        (scale > 0 && dur > 0).then(|| std::time::Duration::from_secs_f64(dur as f64 / scale as f64))
    }

    pub fn parse(file: &mut File) -> Result<Track, String> {
        let (moov, moofs) = read_top(file)?;
        let moov = moov.as_slice();
        let trak = boxes(moov)
            .filter(|(k, _, _)| *k == b"trak")
            .map(|(_, b, _)| b)
            .find(|t| child(t, b"mdia").and_then(|m| child(m, b"hdlr")).and_then(|h| h.get(8..12)) == Some(b"vide"))
            .ok_or("No video track")?;
        let mdia = child(trak, b"mdia").ok_or(BAD)?;
        let mdhd = child(mdia, b"mdhd").ok_or(BAD)?;
        let timescale = if mdhd.first() == Some(&1) { be32(mdhd, 20) } else { be32(mdhd, 12) }.ok_or(BAD)?;
        let stbl = child(mdia, b"minf").and_then(|m| child(m, b"stbl")).ok_or(BAD)?;
        let stsd = child(stbl, b"stsd").ok_or(BAD)?;
        let avc1 = boxes(stsd.get(8..).ok_or(BAD)?)
            .find(|(k, _, _)| *k == b"avc1" || *k == b"avc3")
            .map(|(_, b, _)| b)
            .ok_or("Video isn't H.264")?;
        // avcC sits after the 78-byte visual sample entry.
        let avcc = child(avc1.get(78..).ok_or(BAD)?, b"avcC").ok_or(BAD)?;
        let nal_len = (*avcc.get(4).ok_or(BAD)? as usize & 3) + 1;
        let mut params = Vec::new();
        let mut i = 5;
        for mask in [0x1F, 0xFF] {
            let count = *avcc.get(i).ok_or(BAD)? as usize & mask;
            i += 1;
            for _ in 0..count {
                let len = be16(avcc, i).ok_or(BAD)? as usize;
                params.push(avcc.get(i + 2..i + 2 + len).ok_or(BAD)?.to_vec());
                i += 2 + len;
            }
        }

        let mut samples = sample_table(stbl)?;
        if samples.is_empty() {
            samples = fragments(moov, &moofs)?;
        }
        if samples.is_empty() {
            return Err("Video has no frames".into());
        }

        // Edit list: where the track starts on the timeline (B-frames
        // usually push the first frame's pts out by a frame or two).
        let movie_scale = child(moov, b"mvhd")
            .and_then(|m| if m.first() == Some(&1) { be32(m, 20) } else { be32(m, 12) })
            .unwrap_or(timescale)
            .max(1);
        if let Some(elst) = child(trak, b"edts").and_then(|e| child(e, b"elst")) {
            let v1 = elst.first() == Some(&1);
            let count = be32(elst, 4).unwrap_or(0) as usize;
            let entry = if v1 { 20 } else { 12 };
            let mut delay: i64 = 0;
            for n in 0..count.min(8) {
                let j = 8 + n * entry;
                let (dur, media) = if v1 {
                    (be64(elst, j), be64(elst, j + 8).map(|m| m as i64))
                } else {
                    (be32(elst, j).map(u64::from), be32(elst, j + 4).map(|m| m as i32 as i64))
                };
                let (Some(dur), Some(media)) = (dur, media) else { break };
                if media < 0 {
                    // Empty edit: the video starts this much later.
                    delay += (dur as i128 * timescale as i128 / movie_scale as i128) as i64;
                } else {
                    let shift = delay - media;
                    for s in &mut samples {
                        s.pts += shift;
                    }
                    break;
                }
            }
        }
        Ok(Track { timescale, nal_len, params, samples })
    }

    /// Samples from an ordinary MP4's tables (stts, ctts, stsz, stsc,
    /// stco/co64, stss). Empty for a fragmented file.
    fn sample_table(stbl: &[u8]) -> Result<Vec<Sample>, String> {
        let Some(stsz) = child(stbl, b"stsz") else { return Ok(Vec::new()) };
        let fixed = be32(stsz, 4).ok_or(BAD)?;
        let count = be32(stsz, 8).ok_or(BAD)? as usize;
        if count == 0 {
            return Ok(Vec::new());
        }
        let size = |n: usize| if fixed != 0 { Some(fixed) } else { be32(stsz, 12 + n * 4) };

        // Chunk offsets.
        let chunks: Vec<u64> = if let Some(stco) = child(stbl, b"stco") {
            let n = be32(stco, 4).ok_or(BAD)? as usize;
            (0..n).map(|c| be32(stco, 8 + c * 4).map(u64::from)).collect::<Option<_>>().ok_or(BAD)?
        } else if let Some(co64) = child(stbl, b"co64") {
            let n = be32(co64, 4).ok_or(BAD)? as usize;
            (0..n).map(|c| be64(co64, 8 + c * 8)).collect::<Option<_>>().ok_or(BAD)?
        } else {
            return Err(BAD.into());
        };
        // (first chunk, samples per chunk), first chunk 1-based.
        let stsc = child(stbl, b"stsc").ok_or(BAD)?;
        let runs: Vec<(u32, u32)> = (0..be32(stsc, 4).ok_or(BAD)? as usize)
            .map(|r| Some((be32(stsc, 8 + r * 12)?, be32(stsc, 12 + r * 12)?)))
            .collect::<Option<_>>()
            .ok_or(BAD)?;

        let mut samples = Vec::with_capacity(count);
        let mut run = 0;
        'chunks: for (c, &chunk_offset) in chunks.iter().enumerate() {
            let chunk = c as u32 + 1;
            while run + 1 < runs.len() && runs[run + 1].0 <= chunk {
                run += 1;
            }
            let per_chunk = runs.get(run).map_or(0, |r| r.1);
            let mut offset = chunk_offset;
            for _ in 0..per_chunk {
                if samples.len() == count {
                    break 'chunks;
                }
                let sz = size(samples.len()).ok_or(BAD)?;
                samples.push(Sample { pts: 0, offset, size: sz, keyframe: true });
                offset += sz as u64;
            }
        }

        // Decode times, plus composition offsets for B-frames.
        let stts = child(stbl, b"stts").ok_or(BAD)?;
        let mut dts: i64 = 0;
        let mut n = 0;
        for e in 0..be32(stts, 4).ok_or(BAD)? as usize {
            let (run, delta) = (be32(stts, 8 + e * 8).ok_or(BAD)?, be32(stts, 12 + e * 8).ok_or(BAD)?);
            for _ in 0..run {
                if let Some(s) = samples.get_mut(n) {
                    s.pts = dts;
                }
                n += 1;
                dts += delta as i64;
            }
        }
        if let Some(ctts) = child(stbl, b"ctts") {
            let mut n = 0;
            for e in 0..be32(ctts, 4).ok_or(BAD)? as usize {
                let run = be32(ctts, 8 + e * 8).ok_or(BAD)?;
                // Signed in practice, whatever the version says.
                let off = be32(ctts, 12 + e * 8).ok_or(BAD)? as i32 as i64;
                for _ in 0..run {
                    if let Some(s) = samples.get_mut(n) {
                        s.pts += off;
                    }
                    n += 1;
                }
            }
        }
        // No stss: every frame is a keyframe.
        if let Some(stss) = child(stbl, b"stss") {
            for s in &mut samples {
                s.keyframe = false;
            }
            for e in 0..be32(stss, 4).ok_or(BAD)? as usize {
                let k = be32(stss, 8 + e * 4).ok_or(BAD)? as usize;
                if let Some(s) = k.checked_sub(1).and_then(|k| samples.get_mut(k)) {
                    s.keyframe = true;
                }
            }
        }
        Ok(samples)
    }

    /// Samples from a fragmented MP4's `moof`s (with their file offsets).
    fn fragments(moov: &[u8], moofs: &[(u64, Vec<u8>)]) -> Result<Vec<Sample>, String> {
        // Defaults for the fragments (moov/mvex/trex).
        let trex = child(moov, b"mvex").and_then(|m| child(m, b"trex"));
        let trex_duration = trex.and_then(|t| be32(t, 12)).unwrap_or(0);
        let trex_size = trex.and_then(|t| be32(t, 16)).unwrap_or(0);
        let trex_flags = trex.and_then(|t| be32(t, 20)).unwrap_or(0);

        let mut samples = Vec::new();
        let mut dts: u64 = 0;
        for (moof_start, moof) in moofs {
            let moof_start = *moof_start;
            for (k, traf, _) in boxes(moof) {
                if k != b"traf" {
                    continue;
                }
                let tfhd = child(traf, b"tfhd").ok_or(BAD)?;
                let flags = be32(tfhd, 0).ok_or(BAD)? & 0xFF_FFFF;
                let mut j = 8; // version/flags, track id
                let mut base = moof_start;
                if flags & 0x1 != 0 {
                    base = be64(tfhd, j).ok_or(BAD)?;
                    j += 8;
                }
                if flags & 0x2 != 0 {
                    j += 4;
                }
                let (mut def_duration, mut def_size, mut def_flags) = (trex_duration, trex_size, trex_flags);
                if flags & 0x8 != 0 {
                    def_duration = be32(tfhd, j).ok_or(BAD)?;
                    j += 4;
                }
                if flags & 0x10 != 0 {
                    def_size = be32(tfhd, j).ok_or(BAD)?;
                    j += 4;
                }
                if flags & 0x20 != 0 {
                    def_flags = be32(tfhd, j).ok_or(BAD)?;
                }
                if let Some(tfdt) = child(traf, b"tfdt") {
                    dts = if tfdt.first() == Some(&1) { be64(tfdt, 4) } else { be32(tfdt, 4).map(u64::from) }
                        .ok_or(BAD)?;
                }
                for (k, trun, _) in boxes(traf) {
                    if k != b"trun" {
                        continue;
                    }
                    let version = trun.first().copied().unwrap_or(0);
                    let tflags = be32(trun, 0).ok_or(BAD)? & 0xFF_FFFF;
                    let count = be32(trun, 4).ok_or(BAD)?;
                    let mut j = 8;
                    let mut offset = base;
                    if tflags & 0x1 != 0 {
                        offset = base.wrapping_add_signed(be32(trun, j).ok_or(BAD)? as i32 as i64);
                        j += 4;
                    }
                    let mut first_flags = None;
                    if tflags & 0x4 != 0 {
                        first_flags = be32(trun, j);
                        j += 4;
                    }
                    for n in 0..count {
                        let (mut duration, mut size) = (def_duration, def_size);
                        let mut sflags = if n == 0 { first_flags.unwrap_or(def_flags) } else { def_flags };
                        let mut cto: i64 = 0;
                        if tflags & 0x100 != 0 {
                            duration = be32(trun, j).ok_or(BAD)?;
                            j += 4;
                        }
                        if tflags & 0x200 != 0 {
                            size = be32(trun, j).ok_or(BAD)?;
                            j += 4;
                        }
                        if tflags & 0x400 != 0 {
                            sflags = be32(trun, j).ok_or(BAD)?;
                            j += 4;
                        }
                        if tflags & 0x800 != 0 {
                            let raw = be32(trun, j).ok_or(BAD)?;
                            cto = if version == 0 { raw as i64 } else { raw as i32 as i64 };
                            j += 4;
                        }
                        samples.push(Sample {
                            pts: dts as i64 + cto,
                            offset,
                            size,
                            // sample_is_non_sync_sample
                            keyframe: sflags & 0x1_0000 == 0,
                        });
                        offset += size as u64;
                        dts += duration as u64;
                    }
                }
            }
        }
        Ok(samples)
    }

    /// One sample (length-prefixed NAL units) as Annex B (start codes),
    /// which is what OpenH264 reads.
    pub fn annex_b(data: &[u8], nal_len: usize, out: &mut Vec<u8>) {
        let mut i = 0;
        while i + nal_len <= data.len() {
            let len = data[i..i + nal_len].iter().fold(0usize, |n, b| (n << 8) | *b as usize);
            i += nal_len;
            let Some(nal) = data.get(i..i + len) else { break };
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
            i += len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b() {
        let mut out = Vec::new();
        mp4::annex_b(&[0, 0, 0, 2, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC, 0, 0], 4, &mut out);
        assert_eq!(out, [0, 0, 0, 1, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC]);
    }

    #[test]
    fn not_a_video() {
        let path = std::env::temp_dir().join(format!("bman-not-a-video-{}", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();
        let result = mp4::parse(&mut std::fs::File::open(&path).unwrap());
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err());
    }

    /// `BMAN_TEST_VIDEO=path/to/160.mp4 cargo test video_file -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn video_file() {
        let path = PathBuf::from(std::env::var("BMAN_TEST_VIDEO").expect("BMAN_TEST_VIDEO"));
        let mut video = Video::open(&path, Duration::ZERO);
        let mut shown = 0;
        let mut last = None;
        for step in 0..400 {
            let pos = Duration::from_millis(step * 40); // 16 s at 25 fps
            if let Some(px) = video.frame(pos) {
                assert_eq!(px.len(), VIDEO_W * VIDEO_H);
                if last != Some(px[VIDEO_W * 60 + 120]) {
                    shown += 1;
                    last = Some(px[VIDEO_W * 60 + 120]);
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        println!("{shown} changing frames in 16 s");
        assert!(shown > 50);
        // A jump far ahead reopens there.
        let mut got = false;
        for _ in 0..200 {
            if video.frame(Duration::from_secs(120)).is_some() {
                got = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(got, "no frame after jumping to 2:00");
    }
}
