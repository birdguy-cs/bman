use rodio::{Decoder, DeviceSinkBuilder, Player, Source};
use rppal::pwm::{Channel, Polarity, Pwm};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    /// Play through the queue once, then stop.
    Off,
    /// Repeat the current track.
    Track,
    /// Loop the whole queue (album, or all singles).
    Queue,
}

impl LoopMode {
    pub fn next(self) -> Self {
        match self {
            LoopMode::Off => LoopMode::Track,
            LoopMode::Track => LoopMode::Queue,
            LoopMode::Queue => LoopMode::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LoopMode::Off => "Off",
            LoopMode::Track => "Track",
            LoopMode::Queue => "All",
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

    /// Stop whatever is playing and start `path`.
    pub fn play(&mut self, path: PathBuf) {
        self.play_from(path, Duration::ZERO);
    }

    /// Like `play`, but start `start` into the track (used to carry on where
    /// it was after switching outputs). Starts from 0 if seeking fails.
    pub fn play_from(&mut self, path: PathBuf, start: Duration) {
        self.stop(); // also waits for the old thread to release the PWM pins

        let state = Arc::new(TrackState::default());
        let thread_state = state.clone();
        let sys_audio = self.sys_audio;
        let volume = self.volume.clone();

        self.handle = Some(thread::spawn(move || {
            let result = if sys_audio {
                run_system(&path, start, &volume, &thread_state)
            } else {
                run_gpio(&path, start, &volume, &thread_state)
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
    volume: &AtomicU32,
    state: &TrackState,
) -> Result<(), AudioError> {
    // Open and decode the file first, so a file that can't be read fails
    // without ever touching the audio device.
    let source = Decoder::try_from(File::open(path)?)?;
    if let Some(length) = source.total_duration() {
        state.length_ms.store(length.as_millis() as u64, Ordering::Relaxed);
    }

    // The sink must stay alive for the whole track. Dropping it at the end
    // is expected, so don't log that.
    let mut sink = DeviceSinkBuilder::open_default_sink()?;
    sink.log_on_drop(false);
    let player = Player::connect_new(&sink.mixer());
    player.set_volume(f32::from_bits(volume.load(Ordering::Relaxed)));
    player.append(source);
    if !start.is_zero() {
        let _ = player.try_seek(start);
    }

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
            .store(player.get_pos().as_millis() as u64, Ordering::Relaxed);
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
    volume: &AtomicU32,
    state: &TrackState,
) -> Result<(), AudioError> {
    let mut source = Decoder::try_from(File::open(path)?)?;
    let start_ms = if !start.is_zero() && source.try_seek(start).is_ok() {
        start.as_millis() as u64
    } else {
        0
    };
    let channels = (u16::from(source.channels()) as usize).max(1);
    let src_rate = u32::from(source.sample_rate()).max(1) as u64;
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
            .store(start_ms + n * 1000 / rate, Ordering::Relaxed);

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
