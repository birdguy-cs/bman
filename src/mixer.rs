//! System (output) volume through the ALSA mixer of the "default" device.
//! With PipeWire (`pipewire-alsa`) or PulseAudio, that mixer's "Master" is
//! the sound server's current output, e.g. Bluetooth earbuds, so this is the
//! same volume `wpctl` / `pactl` change. On plain ALSA it's the sound card's
//! own control. No external commands; libasound is already a dependency.
//!
//! The mixer is opened fresh for every call, because the output behind
//! "default" changes (earbuds connecting) and a long-lived handle wouldn't
//! notice.

use alsa::mixer::{Mixer, Selem, SelemChannelId};

/// Controls tried first, in order; otherwise the first with a playback volume.
const PREFERRED: [&str; 5] = ["Master", "PCM", "Speaker", "Headphone", "Digital"];

fn with_control<T>(f: impl FnOnce(&Selem) -> Result<T, String>) -> Result<T, String> {
    let mixer = Mixer::new("default", false).map_err(|e| format!("No mixer: {e}"))?;
    let controls: Vec<Selem> = mixer
        .iter()
        .filter_map(Selem::new)
        .filter(|s| s.has_playback_volume())
        .collect();
    let name = |s: &Selem| s.get_id().get_name().map(str::to_string).unwrap_or_default();
    let control = PREFERRED
        .iter()
        .find_map(|want| controls.iter().find(|s| name(s) == *want))
        .or(controls.first())
        .ok_or("No volume control")?;
    f(control)
}

/// System volume in percent, or an error if there's no usable mixer.
pub fn get() -> Result<u8, String> {
    with_control(|s| {
        let (min, max) = s.get_playback_volume_range();
        let value = s
            .get_playback_volume(SelemChannelId::mono())
            .map_err(|e| e.to_string())?;
        if max <= min {
            return Ok(100);
        }
        Ok((((value - min) * 100 + (max - min) / 2) / (max - min)).clamp(0, 100) as u8)
    })
}

/// Set the system volume (percent, all channels) and unmute it.
pub fn set(percent: u8) -> Result<(), String> {
    with_control(|s| {
        let (min, max) = s.get_playback_volume_range();
        let value = min + (max - min) * percent.min(100) as i64 / 100;
        s.set_playback_volume_all(value).map_err(|e| e.to_string())?;
        if s.has_playback_switch() {
            let _ = s.set_playback_switch_all(1);
        }
        Ok(())
    })
}

#[cfg(test)]
mod live {
    /// Reads (and restores) the real system volume:
    /// `cargo test mixer_roundtrip -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn mixer_roundtrip() {
        let before = super::get().expect("mixer");
        println!("system volume: {before}%");
        let probe = if before >= 50 { before - 5 } else { before + 5 };
        super::set(probe).unwrap();
        println!("after set {probe}: {}%", super::get().unwrap());
        super::set(before).unwrap();
        println!("restored: {}%", super::get().unwrap());
    }
}
