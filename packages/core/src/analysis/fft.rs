//! Short-time Fourier transform shared by every spectral analyzer.
//!
//! Audio is real, so a real-input FFT does half the work of a complex one. The
//! plan, window and buffers are made once per [`Stft`] and reused for every frame:
//! a track is tens of thousands of frames, and allocating per frame used to cost
//! as much as the transforms themselves.

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use std::f32::consts::PI;
use std::sync::Arc;

/// Frequency band boundaries for low/mid/high splits.
#[derive(Debug, Clone, Copy)]
pub struct FrequencyBands {
    /// Upper bound of the low (bass) band in Hz.
    pub low_cutoff: f32,
    /// Lower bound of the high (treble) band in Hz.
    pub high_cutoff: f32,
}

impl Default for FrequencyBands {
    fn default() -> Self {
        Self {
            low_cutoff: 200.0,   // Bass: < 200 Hz
            high_cutoff: 4000.0, // Treble: > 4 kHz
        }
    }
}

/// A Hann-windowed STFT over mono samples.
///
/// Frame `i` is centred on sample `i * hop`, so its time is exactly
/// `i * hop / sample_rate`. Samples outside the signal read as silence.
pub struct Stft {
    frame: usize,
    hop: usize,
    window: Vec<f32>,
    fft: Arc<dyn RealToComplex<f32>>,
    input: Vec<f32>,
    output: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
}

impl Stft {
    pub fn new(frame: usize, hop: usize) -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(frame);
        let window = (0..frame)
            .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f32 / frame as f32).cos()))
            .collect();
        Self {
            frame,
            hop,
            window,
            input: fft.make_input_vec(),
            output: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            fft,
        }
    }

    /// Number of magnitude bins per frame.
    pub fn bins(&self) -> usize {
        self.frame / 2 + 1
    }

    /// Width of one bin in Hz.
    pub fn bin_hz(&self, sample_rate: u32) -> f32 {
        sample_rate as f32 / self.frame as f32
    }

    /// Number of frames whose centre lies inside `len` samples.
    pub fn frame_count(&self, len: usize) -> usize {
        len.div_ceil(self.hop)
    }

    /// Magnitude spectrum of the frame centred on `centre`, written into `mags`
    /// (which must hold [`bins`](Self::bins) values).
    pub fn magnitudes_at(&mut self, samples: &[f32], centre: usize, mags: &mut [f32]) {
        let start = centre as isize - (self.frame / 2) as isize;
        for (i, slot) in self.input.iter_mut().enumerate() {
            let at = start + i as isize;
            let sample = if at >= 0 {
                samples.get(at as usize).copied().unwrap_or(0.0)
            } else {
                0.0
            };
            *slot = sample * self.window[i];
        }
        // Lengths are the planner's own, so processing cannot fail.
        let _ = self
            .fft
            .process_with_scratch(&mut self.input, &mut self.output, &mut self.scratch);
        for (m, c) in mags.iter_mut().zip(&self.output) {
            *m = c.norm();
        }
    }

    /// Call `f(frame_index, magnitudes)` for every frame of `samples`.
    pub fn for_each_frame(&mut self, samples: &[f32], mut f: impl FnMut(usize, &[f32])) {
        let mut mags = vec![0.0f32; self.bins()];
        for i in 0..self.frame_count(samples.len()) {
            self.magnitudes_at(samples, i * self.hop, &mut mags);
            f(i, &mags);
        }
    }
}

/// Normalize a vector of values to 0.0-1.0 range.
pub fn normalize(values: &mut [f32]) {
    let max = values.iter().copied().fold(0.0f32, f32::max).max(0.001);
    for v in values {
        *v /= max;
    }
}

/// Moving average over a centred window of `width` values, in linear time.
pub fn moving_average(signal: &[f32], width: usize) -> Vec<f32> {
    let half = width / 2;
    let mut prefix = Vec::with_capacity(signal.len() + 1);
    prefix.push(0.0f64);
    for &v in signal {
        prefix.push(prefix[prefix.len() - 1] + v as f64);
    }
    (0..signal.len())
        .map(|i| {
            let start = i.saturating_sub(half);
            let end = (i + half + 1).min(signal.len());
            ((prefix[end] - prefix[start]) / (end - start) as f64) as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sine_peaks_in_its_own_bin() {
        let rate = 44_100;
        let samples: Vec<f32> = (0..8192)
            .map(|n| (2.0 * PI * 1000.0 * n as f32 / rate as f32).sin())
            .collect();
        let mut stft = Stft::new(2048, 512);
        let mut mags = vec![0.0; stft.bins()];
        stft.magnitudes_at(&samples, 4096, &mut mags);

        let peak = mags
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap();
        assert!((peak as f32 * stft.bin_hz(rate) - 1000.0).abs() < stft.bin_hz(rate));
    }

    #[test]
    fn frames_cover_the_signal_from_its_first_sample() {
        let stft = Stft::new(1024, 256);
        assert_eq!(stft.frame_count(1024), 4);
        assert_eq!(stft.frame_count(1025), 5);
    }

    #[test]
    fn moving_average_matches_the_naive_one() {
        let signal = [1.0, 2.0, 3.0, 4.0, 5.0];
        let avg = moving_average(&signal, 3);
        assert_eq!(avg, vec![1.5, 2.0, 3.0, 4.0, 4.5]);
    }
}
