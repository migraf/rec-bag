//! Onset strength: how much new sound starts at each moment of a track.
//!
//! A drum hit raises the energy of many frequency bins at once, while a held
//! note or pad changes little from one frame to the next. Summing only the
//! *increases* between consecutive spectra (spectral flux) therefore peaks on
//! percussion. Magnitudes are log-compressed first, so a quiet hi-hat over a
//! loud bass line still registers instead of being drowned by the bass's level.

use super::fft::{moving_average, Stft};

/// Frame length: ~23 ms at 44.1 kHz, short enough to place a hit.
const FRAME: usize = 1024;
/// Frame advance: ~5.8 ms at 44.1 kHz.
const HOP: usize = 256;
/// Strength of the log compression applied to normalised magnitudes.
const COMPRESSION: f32 = 1000.0;
/// Width of the local mean removed from the flux, in seconds. Long enough to
/// span a beat, so steady-state loudness drops out and only hits remain.
const LOCAL_MEAN_SECS: f64 = 0.4;

/// Onset strength, one value per hop, normalised to a peak of 1.
#[derive(Debug, Clone)]
pub struct OnsetEnvelope {
    pub values: Vec<f32>,
    /// Envelope values per second.
    pub rate: f64,
}

impl OnsetEnvelope {
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Seconds into the track of (possibly fractional) envelope index `x`.
    ///
    /// The flux at index `i` compares the frames centred on `i - 1` and `i`, so
    /// it measures what started between the two: half a hop earlier.
    pub fn time_of(&self, x: f64) -> f64 {
        (x - 0.5) / self.rate
    }

    /// Envelope index of a time in seconds; the inverse of [`time_of`](Self::time_of).
    pub fn index_of(&self, secs: f64) -> f64 {
        secs * self.rate + 0.5
    }

    /// The envelope between samples, by linear interpolation; zero outside.
    pub fn sample_at(&self, x: f64) -> f32 {
        if x < 0.0 {
            return 0.0;
        }
        let i = x as usize;
        let frac = (x - i as f64) as f32;
        let a = self.values.get(i).copied().unwrap_or(0.0);
        let b = self.values.get(i + 1).copied().unwrap_or(0.0);
        a + (b - a) * frac
    }

    /// A copy smoothed over `width` values, for searches that step coarser
    /// than one value and must not fall between two halves of a peak.
    pub fn smoothed(&self, width: usize) -> Self {
        Self {
            values: moving_average(&self.values, width),
            rate: self.rate,
        }
    }
}

/// Compute the onset envelope of mono samples.
pub fn onset_envelope(samples: &[f32], sample_rate: u32) -> OnsetEnvelope {
    let rate = sample_rate as f64 / HOP as f64;
    let mut stft = Stft::new(FRAME, HOP);
    // A full-scale sine peaks at a quarter of the frame under a Hann window.
    let scale = COMPRESSION / (FRAME as f32 / 4.0);

    let mut prev: Vec<f32> = vec![0.0; stft.bins()];
    let mut flux = Vec::with_capacity(stft.frame_count(samples.len()));
    stft.for_each_frame(samples, |i, mags| {
        let mut sum = 0.0f32;
        for (p, &m) in prev.iter_mut().zip(mags) {
            let level = (1.0 + scale * m).ln();
            if i > 0 {
                sum += (level - *p).max(0.0);
            }
            *p = level;
        }
        flux.push(sum);
    });

    let local_mean = moving_average(&flux, ((rate * LOCAL_MEAN_SECS) as usize).max(3));
    let mut values: Vec<f32> = flux
        .iter()
        .zip(&local_mean)
        .map(|(f, m)| (f - m).max(0.0))
        .collect();
    super::fft::normalize(&mut values);

    OnsetEnvelope { values, rate }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::test_signals::click_track;

    #[test]
    fn onsets_peak_at_the_clicks() {
        let rate = 44_100;
        let samples = click_track(rate, 120.0, 0.25, 8.0);
        let env = onset_envelope(&samples, rate);

        // Every click (every 0.5 s from 0.25 s) has a strong peak within ~6 ms.
        for k in 0..15 {
            let t = 0.25 + k as f64 * 0.5;
            let centre = env.index_of(t).round() as usize;
            let near = env.values[centre - 1..=centre + 1]
                .iter()
                .copied()
                .fold(0.0f32, f32::max);
            assert!(near > 0.5, "click {k} at {t}s: {near}");
        }
        // And between clicks there is nothing.
        let between = env.index_of(0.5).round() as usize;
        assert!(env.values[between] < 0.05);
    }
}
