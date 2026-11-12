use rodio::{ChannelCount, Decoder, DeviceSinkBuilder, Player, SampleRate, Source};
use rppal::pwm::{Channel, Polarity, Pwm};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, TryRecvError},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type AudioError = Box<dyn std::error::Error + Send + Sync>;

/// Rate the GPIO path plays at. Each frame needs two sysfs PWM writes
/// (one per channel), so this is limited by how fast the Pi can do that.
/// 8000 is a safe start; try 11025 or 16000 and listen for dropouts.
const GPIO_SAMPLE_RATE: u32 = 8_000;

/// PWM carrier frequency. Must be well above GPIO_SAMPLE_RATE.
const GPIO_CARRIER_HZ: f64 = 50_000.0;

/// Samples per chunk the decoder thread hands over, and how many chunks it
/// may get ahead (about 1.5 s of CD-quality stereo).
const PREFETCH_CHUNK: usize = 4096;
const PREFETCH_CHUNKS: usize = 16;

/// Output buffer for the system path, in frames (~170 ms at 48 kHz). rodio's
/// default is 50 ms, which a busy Pi Zero can miss.
const SYSTEM_BUFFER_FRAMES: u32 = 8192;

/// Mix every channel down to one and play it on all of them (Settings >
/// Volume > Sound). Read per frame, so it switches mid-song.
static MONO: AtomicBool = AtomicBool::new(false);

pub fn set_mono(on: bool) {
    MONO.store(on, Ordering::Relaxed);
}

/// Decodes a track on its own thread, ahead of playback, into a bounded
/// queue. Playback only copies samples out, so reading the SD card and
/// decoding bursts (a FLAC/MP3 packet at a time) never stall it. If the
/// decoder ever falls behind, whole frames of silence are played instead of
/// waiting.
struct Prefetch {
    rx: Receiver<Vec<f32>>,
    chunk: Vec<f32>,
    pos: usize,
    channels: ChannelCount,
    sample_rate: SampleRate,
    total: Option<Duration>,
    /// Silent samples still to play to finish a whole frame.
    silence: usize,
    done: bool,
    /// Wait for the decoder instead of playing silence (tests only: they
    /// read much faster than real time).
    #[cfg(test)]
    wait: bool,
}

impl Prefetch {
    /// Open `path`, seek to `start` (if possible) and start decoding ahead.
    /// Returns the stream and whether the seek worked.
    /// `speed` other than 1 time-stretches (stretch.rs: faster or slower,
    /// same pitch), for podcasts and audiobooks.
    fn open(path: &Path, start: Duration, speed: f64) -> Result<(Self, bool), AudioError> {
        let mut decoder = open_decoder(path)?;
        let seeked = !start.is_zero() && decoder.try_seek(start).is_ok();
        let channels = decoder.channels();
        let sample_rate = decoder.sample_rate();
        let total = if crate::video::is_video_file(path) {
            crate::video::duration(path).or(decoder.total_duration())
        } else {
            decoder.total_duration()
        };

        let ch = channels.get() as usize;
        let chunk_len = (PREFETCH_CHUNK / ch).max(1) * ch; // whole frames
        let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(PREFETCH_CHUNKS);
        let mut stretch = ((speed - 1.0).abs() > 0.01)
            .then(|| crate::stretch::Stretch::new(speed, ch, sample_rate.get()));
        thread::Builder::new().name("bman-decode".into()).spawn(move || {
            loop {
                let chunk: Vec<f32> = decoder.by_ref().take(chunk_len).collect();
                let last = chunk.len() < chunk_len;
                let chunk = match stretch.as_mut() {
                    Some(st) => {
                        let mut out = Vec::with_capacity((chunk.len() as f64 / speed) as usize + 4096);
                        st.process(&chunk, &mut out);
                        if last {
                            st.finish(&mut out);
                        }
                        out
                    }
                    None => chunk,
                };
                // Playback stopped (the receiver is gone): stop decoding.
                if !chunk.is_empty() && tx.send(chunk).is_err() {
                    return;
                }
                if last {
                    return;
                }
            }
        })?;

        // Wait for the first chunk here (not in playback) so it starts clean.
        let (chunk, done) = match rx.recv() {
            Ok(chunk) => (chunk, false),
            Err(_) => (Vec::new(), true),
        };
        Ok((
            Self {
                rx,
                chunk,
                pos: 0,
                channels,
                sample_rate,
                total,
                silence: 0,
                done,
                #[cfg(test)]
                wait: false,
            },
            seeked,
        ))
    }
}

impl Iterator for Prefetch {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        loop {
            if self.pos < self.chunk.len() {
                let ch = self.channels.get() as usize;
                // Start of a frame: average it in place when mono is on.
                if ch > 1 && self.pos % ch == 0 && MONO.load(Ordering::Relaxed) {
                    if let Some(frame) = self.chunk.get_mut(self.pos..self.pos + ch) {
                        let avg = frame.iter().sum::<f32>() / ch as f32;
                        frame.fill(avg);
                    }
                }
                self.pos += 1;
                return Some(self.chunk[self.pos - 1]);
            }
            if self.silence > 0 {
                self.silence -= 1;
                return Some(0.0);
            }
            if self.done {
                return None;
            }
            #[cfg(test)]
            if self.wait {
                match self.rx.recv() {
                    Ok(chunk) => {
                        self.chunk = chunk;
                        self.pos = 0;
                    }
                    Err(_) => self.done = true,
                }
                continue;
            }
            match self.rx.try_recv() {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                // Decoder behind (shouldn't happen): a frame of silence
                // rather than blocking the audio callback.
                Err(TryRecvError::Empty) => {
                    self.silence = self.channels.get() as usize - 1;
                    return Some(0.0);
                }
                Err(TryRecvError::Disconnected) => self.done = true,
            }
        }
    }
}

impl Source for Prefetch {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.total
    }
}

/// rodio's decoder for everything, except MP4-family files (.m4a / .m4b /
/// .mp4 / .aac), which get `TrackDecoder`: rodio's hands the packets of
/// every track to the audio decoder, so an audiobook's chapter-title track
/// ends playback at the second chapter.
fn open_decoder(path: &Path) -> Result<Box<dyn Source + Send>, AudioError> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ["m4a", "m4b", "mp4", "aac"].contains(&ext.as_str()) {
        match TrackDecoder::open(path, &ext) {
            Ok(d) => return Ok(Box::new(d)),
            Err(e) => warn!("{}: {e}; trying rodio's decoder", path.display()),
        }
    }
    Ok(Box::new(Decoder::try_from(File::open(path)?)?))
}

/// Symphonia, reading only the first audio track's packets.
struct TrackDecoder {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track: u32,
    buffer: Vec<f32>,
    pos: usize,
    channels: ChannelCount,
    sample_rate: SampleRate,
    total: Option<Duration>,
}

impl TrackDecoder {
    fn open(path: &Path, ext: &str) -> Result<Self, AudioError> {
        use symphonia::core::{codecs::DecoderOptions, formats::FormatOptions, io::MediaSourceStream,
                              meta::MetadataOptions, probe::Hint};
        let mss = MediaSourceStream::new(Box::new(File::open(path)?), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(ext);
        let probed = symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions { enable_gapless: true, ..Default::default() },
            &MetadataOptions::default(),
        )?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL && t.codec_params.sample_rate.is_some())
            .ok_or("no audio track")?;
        let (params, track_id) = (track.codec_params.clone(), track.id);
        let total = match (params.n_frames, params.time_base) {
            (Some(n), Some(tb)) => {
                let t = tb.calc_time(n);
                Some(Duration::from_secs(t.seconds) + Duration::from_secs_f64(t.frac))
            }
            _ => None,
        };
        let decoder = symphonia::default::get_codecs().make(&params, &DecoderOptions::default())?;
        let mut d = Self {
            format,
            decoder,
            track: track_id,
            buffer: Vec::new(),
            pos: 0,
            channels: ChannelCount::new(params.channels.map_or(2, |c| c.count() as u16).max(1)).ok_or("channels")?,
            sample_rate: SampleRate::new(params.sample_rate.unwrap_or(44_100)).ok_or("rate")?,
            total,
        };
        // The first packet gives the real layout (AAC can leave it out of
        // the container).
        d.refill();
        Ok(d)
    }

    /// Decode the next packet of our track into `buffer`. False at the end.
    fn refill(&mut self) -> bool {
        use symphonia::core::{audio::SampleBuffer, errors::Error};
        loop {
            let Ok(packet) = self.format.next_packet() else { return false };
            if packet.track_id() != self.track {
                continue; // chapter titles, cover art, ...
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) if decoded.frames() > 0 => {
                    let spec = *decoded.spec();
                    let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                    buf.copy_interleaved_ref(decoded);
                    self.buffer.clear();
                    self.buffer.extend_from_slice(buf.samples());
                    self.pos = 0;
                    if let Some(c) = ChannelCount::new(spec.channels.count() as u16) {
                        self.channels = c;
                    }
                    if let Some(r) = SampleRate::new(spec.rate) {
                        self.sample_rate = r;
                    }
                    return true;
                }
                Ok(_) | Err(Error::DecodeError(_)) => continue,
                Err(_) => return false,
            }
        }
    }
}

impl Iterator for TrackDecoder {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.pos >= self.buffer.len() && !self.refill() {
            return None;
        }
        self.pos += 1;
        self.buffer.get(self.pos - 1).copied()
    }
}

impl Source for TrackDecoder {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.total
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        use symphonia::core::formats::{SeekMode, SeekTo};
        let to = SeekTo::Time { time: pos.as_secs_f64().into(), track_id: Some(self.track) };
        self.format
            .seek(SeekMode::Accurate, to)
            .map_err(|e| rodio::source::SeekError::Other(std::sync::Arc::new(e)))?;
        self.decoder.reset();
        self.buffer.clear();
        self.pos = 0;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    /// Play through the queue once, then stop.
    Off,
    /// Repeat the current track.
    Track,
    /// Loop the whole queue (album, or all singles).
    Queue,
    /// Loop the queue in random order: every song once per round, never
    /// the same song twice in a row.
    Random,
}

impl LoopMode {
    pub fn next(self) -> Self {
        match self {
            LoopMode::Off => LoopMode::Track,
            LoopMode::Track => LoopMode::Queue,
            LoopMode::Queue => LoopMode::Random,
            LoopMode::Random => LoopMode::Off,
        }
    }

    /// Inverse of `label` (Off for anything unknown).
    pub fn from_label(label: &str) -> Self {
        match label {
            "Track" => LoopMode::Track,
            "All" => LoopMode::Queue,
            "Random" => LoopMode::Random,
            _ => LoopMode::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LoopMode::Off => "Off",
            LoopMode::Track => "Track",
            LoopMode::Queue => "All",
            LoopMode::Random => "Random",
        }
    }
}

/// Shared between the UI thread and the thread playing a track.
#[derive(Default)]
struct TrackState {
    stop: AtomicBool,
    paused: AtomicBool,
    finished: AtomicBool,
    failed: AtomicBool,
    position_ms: AtomicU64,
    length_ms: AtomicU64,
}

/// Plays one track at a time on a background thread. Music keeps playing
/// no matter which screen is showing.
///   sys_audio = true  -> system output via rodio (PC testing)
///   sys_audio = false -> stereo hardware PWM on GPIO 12 (L) and GPIO 13 (R)
pub struct AudioPlayer {
    sys_audio: bool,
    /// f32 bits, shared with the playing thread so volume changes apply live.
    volume: Arc<AtomicU32>,
    state: Option<Arc<TrackState>>,
    handle: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    pub fn new(sys_audio: bool, volume: f32) -> Self {
        Self {
            sys_audio,
            volume: Arc::new(AtomicU32::new(volume.clamp(0.0, 1.0).to_bits())),
            state: None,
            handle: None,
        }
    }

    /// Stop whatever is playing and start `path`, `start` into the track
    /// (zero, or where it was after switching outputs or a restart). Starts
    /// from 0 if seeking fails.
    pub fn play_from(&mut self, path: PathBuf, start: Duration) {
        self.play_from_speed(path, start, 1.0);
    }

    /// `play_from` at `speed` (1.0 = normal; podcasts and audiobooks can go
    /// 0.75-2x, same pitch). Positions stay in the file's own time.
    pub fn play_from_speed(&mut self, path: PathBuf, start: Duration, speed: f64) {
        self.stop(); // also waits for the old thread to release the PWM pins

        let state = Arc::new(TrackState::default());
        let thread_state = state.clone();
        let sys_audio = self.sys_audio;
        let volume = self.volume.clone();

        self.handle = Some(thread::spawn(move || {
            let result = if sys_audio {
                run_system(&path, start, speed, &volume, &thread_state)
            } else {
                run_gpio(&path, start, speed, &volume, &thread_state)
            };

            if let Err(e) = result {
                error!("Audio error ({}): {e}", path.display());
                thread_state.failed.store(true, Ordering::SeqCst);
            }
            thread_state.finished.store(true, Ordering::SeqCst);
        }));
        self.state = Some(state);
    }

    pub fn stop(&mut self) {
        if let Some(state) = self.state.take() {
            state.stop.store(true, Ordering::SeqCst);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// 0.0 - 1.0. Takes effect immediately, even mid-track.
    pub fn set_volume(&self, volume: f32) {
        self.volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Switch between rodio (PC testing / Bluetooth) and GPIO PWM.
    /// Takes effect from the next `play`.
    pub fn set_system_audio(&mut self, on: bool) {
        self.sys_audio = on;
    }

    pub fn toggle_pause(&self) {
        if let Some(state) = &self.state {
            state.paused.fetch_xor(true, Ordering::SeqCst);
        }
    }

    pub fn is_paused(&self) -> bool {
        self.state
            .as_ref()
            .map(|s| s.paused.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    pub fn position(&self) -> Duration {
        Duration::from_millis(
            self.state
                .as_ref()
                .map(|s| s.position_ms.load(Ordering::Relaxed))
                .unwrap_or(0),
        )
    }

    pub fn position_secs(&self) -> u64 {
        self.state
            .as_ref()
            .map(|s| s.position_ms.load(Ordering::Relaxed) / 1000)
            .unwrap_or(0)
    }

    /// 0 if the length isn't known (some MP3s don't report one).
    pub fn length_secs(&self) -> u64 {
        self.state
            .as_ref()
            .map(|s| s.length_ms.load(Ordering::Relaxed) / 1000)
            .unwrap_or(0)
    }

    /// Call every frame. Returns Some(failed) once the current track has
    /// ended on its own (or errored), None otherwise.
    pub fn poll_finished(&mut self) -> Option<bool> {
        let state = self.state.as_ref()?;
        if !state.finished.load(Ordering::SeqCst) {
            return None;
        }

        let failed = state.failed.load(Ordering::SeqCst);
        self.state = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        Some(failed)
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// PC testing path: stream the file through rodio.
fn run_system(
    path: &Path,
    start: Duration,
    speed: f64,
    volume: &AtomicU32,
    state: &TrackState,
) -> Result<(), AudioError> {
    // Open the file and start decoding ahead first, so a file that can't be
    // read fails without ever touching the audio device.
    let (source, seeked) = Prefetch::open(path, start, speed)?;
    let offset = if seeked { start } else { Duration::ZERO };
    if let Some(length) = source.total_duration() {
        state.length_ms.store(length.as_millis() as u64, Ordering::Relaxed);
    }

    // A bigger output buffer than rodio's default, falling back to the
    // default if the device won't take it. The sink must stay alive for the
    // whole track; dropping it at the end is expected, so don't log that.
    let mut sink = DeviceSinkBuilder::from_default_device()
        .map(|b| b.with_buffer_size(rodio::cpal::BufferSize::Fixed(SYSTEM_BUFFER_FRAMES)))
        .and_then(|b| b.open_stream())
        .or_else(|_| DeviceSinkBuilder::open_default_sink())?;
    sink.log_on_drop(false);
    let player = Player::connect_new(&sink.mixer());
    player.set_volume(f32::from_bits(volume.load(Ordering::Relaxed)));
    player.append(source);

    while !player.empty() {
        if state.stop.load(Ordering::SeqCst) {
            player.stop();
            break;
        }

        if state.paused.load(Ordering::SeqCst) {
            if !player.is_paused() {
                player.pause();
            }
        } else if player.is_paused() {
            player.play();
        }

        player.set_volume(f32::from_bits(volume.load(Ordering::Relaxed)));
        state
            .position_ms
            .store((offset + player.get_pos().mul_f64(speed)).as_millis() as u64, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(50));
    }

    Ok(())
}

/// Hardware path: decode the file as a stream, downsample to
/// GPIO_SAMPLE_RATE and drive GPIO 12 (L) / GPIO 13 (R) with hardware PWM.
///
/// Needs in /boot/firmware/config.txt:
///   dtoverlay=pwm-2chan,pin=12,func=4,pin2=13,func2=4
///
/// GPIO 12/13 are PWM0/PWM1. GPIO 18/19 (the other PWM pins) are not used
/// because the Whisplay HAT's I2S audio chip needs GPIO 18-21.
fn run_gpio(
    path: &Path,
    start: Duration,
    speed: f64,
    volume: &AtomicU32,
    state: &TrackState,
) -> Result<(), AudioError> {
    let (mut source, seeked) = Prefetch::open(path, start, speed)?;
    let start_ms = if seeked { start.as_millis() as u64 } else { 0 };
    let channels = (source.channels().get() as usize).max(1);
    let src_rate = source.sample_rate().get().max(1) as u64;

    // Every sample is timed by hand here, so ask for a real-time slot.
    if !crate::sched::audio() {
        info!("No real-time priority for GPIO audio (needs root or an rtprio limit)");
    }
    if let Some(length) = source.total_duration() {
        state.length_ms.store(length.as_millis() as u64, Ordering::Relaxed);
    }

    // Silence is 50% duty; the signal swings around that.
    let left = Pwm::with_frequency(Channel::Pwm0, GPIO_CARRIER_HZ, 0.5, Polarity::Normal, true)?;
    let right = Pwm::with_frequency(Channel::Pwm1, GPIO_CARRIER_HZ, 0.5, Polarity::Normal, true)?;

    let rate = GPIO_SAMPLE_RATE as u64;
    let period = Duration::from_secs_f64(1.0 / GPIO_SAMPLE_RATE as f64);

    let (mut l, mut r) = (0.0f32, 0.0f32);
    let mut src_frame: u64 = 0; // source frames consumed so far
    let mut n: u64 = 0; // output frames played so far
    let mut next = Instant::now();

    'play: loop {
        if state.stop.load(Ordering::SeqCst) {
            break;
        }

        if state.paused.load(Ordering::SeqCst) {
            left.set_duty_cycle(0.5)?;
            right.set_duty_cycle(0.5)?;
            thread::sleep(Duration::from_millis(20));
            next = Instant::now(); // don't "catch up" after resuming
            continue;
        }

        // Read source frames up to the one this output frame corresponds to
        // (nearest-neighbour downsampling, streamed so nothing is buffered).
        let target = n * src_rate / rate;
        while src_frame <= target {
            let Some(first) = source.next() else {
                break 'play; // end of track
            };
            let second = if channels >= 2 {
                source.next().unwrap_or(first)
            } else {
                first // mono: same sample on both channels
            };
            for _ in 2..channels {
                source.next(); // ignore extra channels
            }
            l = first;
            r = second;
            src_frame += 1;
        }

        let vol = f32::from_bits(volume.load(Ordering::Relaxed)) as f64;
        left.set_duty_cycle((0.5 + 0.5 * l as f64 * vol).clamp(0.0, 1.0))?;
        right.set_duty_cycle((0.5 + 0.5 * r as f64 * vol).clamp(0.0, 1.0))?;

        n += 1;
        state
            .position_ms
            .store(start_ms + (n as f64 * 1000.0 * speed / rate as f64) as u64, Ordering::Relaxed);

        // Wait for the next frame deadline: sleep most of it, spin the rest.
        next += period;
        let now = Instant::now();
        if next > now {
            let wait = next - now;
            if wait > Duration::from_micros(300) {
                thread::sleep(wait - Duration::from_micros(200));
            }
            while Instant::now() < next {
                std::hint::spin_loop();
            }
        } else if now - next > Duration::from_millis(50) {
            next = now; // fell far behind, resync instead of racing
        }
    }

    let _ = left.set_duty_cycle(0.5);
    let _ = right.set_duty_cycle(0.5);
    left.disable()?;
    right.disable()?;

    Ok(())
}

#[cfg(test)]
mod live {
    use super::*;

    /// Decodes a song both directly and through Prefetch and compares every
    /// sample: `BMAN_TEST_SONG=path cargo test prefetch_matches -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn prefetch_matches() {
        let path = PathBuf::from(std::env::var("BMAN_TEST_SONG").expect("BMAN_TEST_SONG"));
        let direct: Vec<f32> = Decoder::try_from(File::open(&path).unwrap()).unwrap().collect();
        let t = Instant::now();
        let (prefetch, _) = Prefetch::open(&path, Duration::ZERO, 1.0).unwrap();
        let first_chunk = t.elapsed();
        let mut silence_inserted = 0usize;
        let mut via: Vec<f32> = Vec::with_capacity(direct.len());
        let mut p = prefetch;
        p.wait = true;
        // Pull slowly at first so the decoder has to stay ahead.
        while let Some(s) = p.next() {
            via.push(s);
        }
        // Underrun silence would show up as extra samples.
        if via.len() > direct.len() {
            silence_inserted = via.len() - direct.len();
        }
        println!(
            "samples direct {} prefetch {}, first chunk after {:?}, silence {}",
            direct.len(), via.len(), first_chunk, silence_inserted
        );
        let mismatches = direct.iter().zip(&via).filter(|(a, b)| a != b).count();
        println!("mismatching samples: {mismatches}");
    }

    /// Time-stretched decoding gives about length / speed of audio:
    /// `BMAN_TEST_SONG=path cargo test prefetch_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn prefetch_speed() {
        let path = PathBuf::from(std::env::var("BMAN_TEST_SONG").expect("BMAN_TEST_SONG"));
        for speed in [1.0, 1.25, 2.0] {
            let (mut p, _) = Prefetch::open(&path, Duration::ZERO, speed).unwrap();
            p.wait = true;
            let ch = p.channels().get() as u64;
            let rate = p.sample_rate().get() as u64;
            let total = p.total_duration();
            let secs = p.count() as f64 / ch as f64 / rate as f64;
            println!("speed {speed}: {secs:.1} s of audio (file {total:?})");
        }
    }

    /// Seeking through Prefetch lands where asked.
    #[test]
    #[ignore]
    fn prefetch_seek() {
        let path = PathBuf::from(std::env::var("BMAN_TEST_SONG").expect("BMAN_TEST_SONG"));
        let (mut p, seeked) = Prefetch::open(&path, Duration::from_secs(30), 1.0).unwrap();
        p.wait = true;
        let ch = p.channels().get() as u64;
        let rate = p.sample_rate().get() as u64;
        let total = p.total_duration();
        let remaining = p.count() as u64 / ch;
        println!("seeked {seeked}, total {total:?}, remaining {:.1}s", remaining as f64 / rate as f64);
    }
}
