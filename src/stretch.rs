//! Pitch-correct playback speed for podcasts and audiobooks: WSOLA
//! (waveform-similarity overlap-add) time-stretching. The input is cut into
//! overlapping 30 ms frames, taken `speed` times further apart than they
//! are written out; each frame is nudged (up to ±10 ms) to where it lines up
//! best with the previous one, so voices keep their pitch without the
//! warble of plain overlap-add. Pure Rust, no allocations per sample.
//!
//! The search for the best offset runs on a decimated mono mix, so it stays
//! cheap enough for a Pi Zero next to decoding.

/// Frame length and search range, in seconds.
const FRAME_SECS: f64 = 0.030;
const SEARCH_SECS: f64 = 0.010;
/// The offset search looks at every DECIMATE-th frame.
const DECIMATE: usize = 4;
/// Offsets tried in the first pass of the search (then all around the best).
const COARSE: usize = 4;

pub struct Stretch {
    speed: f64,
    channels: usize,
    /// Frame length and output hop, in frames (hop = frame / 2).
    frame: usize,
    hop: usize,
    search: usize,
    window: Vec<f32>,
    /// Interleaved input not consumed yet.
    input: Vec<f32>,
    /// Where the next frame should start in `input` (in frames), before the
    /// similarity nudge.
    nominal: f64,
    /// Where the previous frame actually started in `input`.
    prev: Option<usize>,
    /// Second half of the previous windowed frame, waiting to be overlapped.
    tail: Vec<f32>,
}

impl Stretch {
    pub fn new(speed: f64, channels: usize, sample_rate: u32) -> Self {
        let channels = channels.max(1);
        let frame = ((FRAME_SECS * sample_rate as f64) as usize / 2 * 2).max(16);
        let hop = frame / 2;
        // Periodic Hann: two half-overlapping copies sum to exactly 1.
        let window = (0..frame)
            .map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / frame as f64).cos()) as f32)
            .collect();
        Self {
            speed: speed.clamp(0.25, 4.0),
            channels,
            frame,
            hop,
            search: ((SEARCH_SECS * sample_rate as f64) as usize).max(1),
            window,
            input: Vec::new(),
            nominal: 0.0,
            prev: None,
            tail: vec![0.0; hop * channels],
        }
    }

    /// Feed decoded samples (interleaved) and append what's ready to `out`.
    pub fn process(&mut self, samples: &[f32], out: &mut Vec<f32>) {
        self.input.extend_from_slice(samples);
        let ch = self.channels;
        let (frame, hop) = (self.frame, self.hop);
        loop {
            let nominal = self.nominal.round() as usize;
            let natural = self.prev.map(|p| p + hop);
            // Need the search range and the natural continuation in full.
            let need = (nominal + self.search + frame).max(natural.map_or(0, |n| n + frame));
            if self.input.len() / ch < need {
                break;
            }
            let start = match natural {
                None => nominal,
                Some(natural) => self.best_offset(natural, nominal),
            };
            // Overlap-add: previous tail + first half of this frame, out.
            for i in 0..hop {
                for c in 0..ch {
                    let s = self.input[(start + i) * ch + c] * self.window[i];
                    out.push(self.tail[i * ch + c] + s);
                }
            }
            for i in 0..hop {
                for c in 0..ch {
                    self.tail[i * ch + c] = self.input[(start + hop + i) * ch + c] * self.window[hop + i];
                }
            }
            self.prev = Some(start);
            self.nominal += hop as f64 * self.speed;
            self.compact();
        }
    }

    /// The start near `nominal` whose frame looks most like the one at
    /// `natural` (what would have followed the previous frame).
    fn best_offset(&self, natural: usize, nominal: usize) -> usize {
        let ch = self.channels;
        let mono = |frame: usize| -> f32 {
            let at = frame * ch;
            self.input[at..at + ch].iter().sum::<f32>()
        };
        let score = |offset: usize| -> f32 {
            let (mut dot, mut energy) = (0.0f32, 0.0f32);
            let mut i = 0;
            while i < self.frame {
                let a = mono(natural + i);
                let b = mono(offset + i);
                dot += a * b;
                energy += b * b;
                i += DECIMATE;
            }
            // Normalised, so loud and quiet candidates compare fairly.
            dot / (energy.sqrt() + 1e-6)
        };
        let lo = nominal.saturating_sub(self.search);
        let hi = nominal + self.search;
        // Coarse pass every COARSE offsets, then every offset around the best.
        let (mut best, mut best_score) = (nominal, f32::MIN);
        for offset in (lo..=hi).step_by(COARSE) {
            let s = score(offset);
            if s > best_score {
                (best, best_score) = (offset, s);
            }
        }
        let center = best;
        for offset in center.saturating_sub(COARSE - 1).max(lo)..=(center + COARSE - 1).min(hi) {
            let s = score(offset);
            if s > best_score {
                (best, best_score) = (offset, s);
            }
        }
        best
    }

    /// Drop input that no later frame can reach.
    fn compact(&mut self) {
        let keep_from = (self.nominal as usize)
            .saturating_sub(self.search)
            .min(self.prev.map_or(usize::MAX, |p| p + self.hop));
        if keep_from > 4096 {
            self.input.drain(..keep_from * self.channels);
            self.nominal -= keep_from as f64;
            self.prev = self.prev.map(|p| p - keep_from);
        }
    }

    /// End of the input: let the last overlap out.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        out.extend_from_slice(&self.tail);
        self.tail.iter_mut().for_each(|s| *s = 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, secs: f64, channels: usize) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        (0..n)
            .flat_map(|i| {
                let s = (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32 * 0.5;
                std::iter::repeat_n(s, channels)
            })
            .collect()
    }

    /// Upward zero crossings per second on channel 0: the pitch.
    fn pitch(samples: &[f32], rate: u32, channels: usize) -> f64 {
        let mono: Vec<f32> = samples.chunks(channels).map(|f| f[0]).collect();
        let crossings = mono.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        crossings as f64 / (mono.len() as f64 / rate as f64)
    }

    fn run(speed: f64, input: &[f32], rate: u32, channels: usize) -> Vec<f32> {
        let mut st = Stretch::new(speed, channels, rate);
        let mut out = Vec::new();
        // In decoder-sized chunks, like playback feeds it.
        for chunk in input.chunks(4096) {
            st.process(chunk, &mut out);
        }
        st.finish(&mut out);
        out
    }

    #[test]
    fn faster_is_shorter_and_keeps_pitch() {
        let rate = 44_100;
        for speed in [0.75, 1.25, 1.5, 2.0] {
            let input = sine(440.0, rate, 3.0, 2);
            let out = run(speed, &input, rate, 2);
            let expected = input.len() as f64 / speed;
            let ratio = out.len() as f64 / expected;
            assert!((0.95..1.05).contains(&ratio), "speed {speed}: length ratio {ratio}");
            // Skip the fade-in of the first frame.
            let p = pitch(&out[rate as usize / 10 * 2..], rate, 2);
            assert!((p - 440.0).abs() < 5.0, "speed {speed}: pitch {p}");
            assert_eq!(out.len() % 2, 0, "whole frames");
        }
    }

    #[test]
    fn no_clicks_on_a_steady_tone() {
        // Overlap-add of a well-aligned sine stays smooth: no sample jumps far
        // more than the sine itself does.
        let rate = 16_000;
        let input = sine(200.0, rate, 2.0, 1);
        let out = run(1.5, &input, rate, 1);
        let max_step = 2.0 * std::f32::consts::PI * 200.0 / rate as f32 * 0.5;
        let jumps = out[rate as usize / 10..out.len() - 2000]
            .windows(2)
            .filter(|w| (w[1] - w[0]).abs() > max_step * 1.5)
            .count();
        assert!(jumps < 10, "{jumps} clicks");
    }
}
