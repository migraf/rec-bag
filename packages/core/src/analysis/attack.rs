//! Millisecond-resolution transient maps used to put grid lines on the kick.
//!
//! The onset envelope locates hits to within a few milliseconds, but a DJ grid
//! is judged by eye and ear at the very front of the kick. That front is the
//! beater's click: broadband and only a millisecond or two long. The kick's
//! body, below ~150 Hz, takes a quarter of a period of its own frequency
//! (several ms) just to rise, so it cannot place a beat precisely — but it does
//! say *which* hits are kicks, which is what deciding between the beat and the
//! off-beat needs.

/// Map values per second: one per millisecond.
pub const RATE: f64 = 1000.0;
/// Band the click is measured in: above the kick's body and the bass, below
/// the air of cymbals.
const CLICK_LOW_HZ: f32 = 1000.0;
const CLICK_HIGH_HZ: f32 = 8000.0;
/// Upper edge of the kick body's band.
const KICK_HIGH_HZ: f32 = 150.0;
/// A rise counts as a candidate attack when it is this fraction of the
/// strongest rise near the predicted beat.
const CANDIDATE_RISE: f32 = 0.5;
/// The attack starts where the rise falls below this fraction of its peak.
const RISE_START: f32 = 0.25;

/// RMS of the click and kick-body bands, one value per millisecond.
#[derive(Debug, Clone, Default)]
pub struct Transients {
    click: Vec<f32>,
    kick: Vec<f32>,
    /// Rises below this are noise, not attacks.
    floor: f32,
}

/// One located attack.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Attack {
    /// Seconds into the track where the attack starts.
    pub secs: f64,
    /// Height of its steepest one-millisecond rise.
    pub strength: f32,
}

impl Transients {
    /// Measure both bands over mono samples, in one pass.
    pub fn new(samples: &[f32], sample_rate: u32) -> Self {
        let mut click_hp = Biquad::highpass(sample_rate, CLICK_LOW_HZ);
        let mut click_lp = Biquad::lowpass(sample_rate, CLICK_HIGH_HZ);
        let mut kick_lp1 = Biquad::lowpass(sample_rate, KICK_HIGH_HZ);
        let mut kick_lp2 = Biquad::lowpass(sample_rate, KICK_HIGH_HZ);

        let ms = (samples.len() as f64 * RATE / sample_rate as f64) as usize;
        let mut click = Vec::with_capacity(ms);
        let mut kick = Vec::with_capacity(ms);
        let mut start = 0;
        for i in 0..ms {
            let end = (((i + 1) as f64 * sample_rate as f64 / RATE) as usize).min(samples.len());
            let (mut c2, mut k2) = (0.0f32, 0.0f32);
            for &x in &samples[start..end] {
                let c = click_lp.process(click_hp.process(x));
                let k = kick_lp2.process(kick_lp1.process(x));
                c2 += c * c;
                k2 += k * k;
            }
            let n = (end - start).max(1) as f32;
            click.push((c2 / n).sqrt());
            kick.push((k2 / n).sqrt());
            start = end;
        }

        // Anything under a twentieth of the track's typical strong rise is noise.
        let mut rises: Vec<f32> = click.windows(2).map(|w| w[1] - w[0]).filter(|&d| d > 0.0).collect();
        let floor = if rises.is_empty() {
            f32::INFINITY
        } else {
            let at = rises.len() * 99 / 100;
            0.05 * *rises.select_nth_unstable_by(at, f32::total_cmp).1
        };

        Self { click, kick, floor }
    }

    /// The attack nearest `secs` within `reach` seconds either side.
    ///
    /// Of the rises in the window at least half as steep as the steepest, the
    /// nearest wins: a grid line must not slide towards a sharper hit later in
    /// the window (a clap or snare layered after the kick).
    pub fn attack_near(&self, secs: f64, reach: f64) -> Option<Attack> {
        // Candidates compare their rise with the one before, so start at 2.
        let lo = (((secs - reach) * RATE).floor().max(2.0)) as usize;
        let hi = (((secs + reach) * RATE).ceil() as usize).min(self.click.len().saturating_sub(1));
        if lo >= hi {
            return None;
        }
        let rise = |i: usize| self.click[i] - self.click[i - 1];
        let steepest = (lo..=hi).map(rise).fold(0.0f32, f32::max);
        if steepest < self.floor {
            return None;
        }

        let target = secs * RATE;
        let peak = (lo..=hi)
            .filter(|&i| {
                let r = rise(i);
                r >= CANDIDATE_RISE * steepest
                    && r >= rise(i - 1)
                    && (i + 1 >= self.click.len() || r >= rise(i + 1))
            })
            .min_by(|&a, &b| (a as f64 - target).abs().total_cmp(&(b as f64 - target).abs()))?;

        let height = rise(peak);
        let mut first = peak;
        while first > 1 && peak - first < 10 && rise(first - 1) > RISE_START * height {
            first -= 1;
        }
        Some(Attack {
            // The rise into block `first` happened somewhere inside it.
            secs: (first as f64 + 0.5) / RATE,
            strength: height,
        })
    }

    /// How hard the kick band hits at `secs`: its peak in the 40 ms after,
    /// above its low point in the 40 ms before.
    pub fn kick_at(&self, secs: f64) -> f32 {
        let at = (secs * RATE).round();
        if at < 0.0 || at as usize >= self.kick.len() {
            return 0.0;
        }
        let at = at as usize;
        let before = self.kick[at.saturating_sub(40)..=at]
            .iter()
            .copied()
            .fold(f32::INFINITY, f32::min);
        let after = self.kick[at..(at + 40).min(self.kick.len())]
            .iter()
            .copied()
            .fold(0.0f32, f32::max);
        (after - before).max(0.0)
    }
}

/// A second-order IIR filter (the RBJ audio-EQ cookbook forms).
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    const Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

    fn lowpass(sample_rate: u32, hz: f32) -> Self {
        let (cos, alpha) = Self::prewarp(sample_rate, hz);
        Self::normalised((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0, cos, alpha)
    }

    fn highpass(sample_rate: u32, hz: f32) -> Self {
        let (cos, alpha) = Self::prewarp(sample_rate, hz);
        Self::normalised((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, cos, alpha)
    }

    fn prewarp(sample_rate: u32, hz: f32) -> (f32, f32) {
        // Keep the corner below Nyquist for low sample rates.
        let hz = hz.min(sample_rate as f32 * 0.45);
        let w0 = 2.0 * std::f32::consts::PI * hz / sample_rate as f32;
        (w0.cos(), w0.sin() / (2.0 * Self::Q))
    }

    fn normalised(b0: f32, b1: f32, b2: f32, cos: f32, alpha: f32) -> Self {
        let a0 = 1.0 + alpha;
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::test_signals::{add_hat, add_kick, silence};

    #[test]
    fn finds_the_front_of_a_kick() {
        let rate = 44_100;
        let mut samples = silence(rate, 2.0);
        add_kick(&mut samples, rate, 1.0, 0.8);
        let transients = Transients::new(&samples, rate);

        let attack = transients.attack_near(1.008, 0.015).unwrap();
        assert!((attack.secs - 1.0).abs() < 0.002, "{}", attack.secs);
    }

    #[test]
    fn prefers_the_hit_nearest_the_prediction() {
        let rate = 44_100;
        let mut samples = silence(rate, 2.0);
        add_kick(&mut samples, rate, 1.0, 0.6);
        add_hat(&mut samples, rate, 1.012, 0.9);
        let transients = Transients::new(&samples, rate);

        let attack = transients.attack_near(0.998, 0.015).unwrap();
        assert!((attack.secs - 1.0).abs() < 0.002, "{}", attack.secs);
    }

    #[test]
    fn looking_at_the_very_start_of_the_track_is_safe() {
        let rate = 44_100;
        let mut samples = silence(rate, 2.0);
        add_kick(&mut samples, rate, 0.0, 0.8);
        add_kick(&mut samples, rate, 0.005, 0.8);
        let transients = Transients::new(&samples, rate);

        // A grid's first beat can be predicted at or just before zero.
        for secs in [-0.02, -0.001, 0.0, 0.001, 0.002] {
            transients.attack_near(secs, 0.025);
            transients.kick_at(secs);
        }
    }

    #[test]
    fn silence_has_no_attacks() {
        let rate = 44_100;
        let mut samples = silence(rate, 2.0);
        add_kick(&mut samples, rate, 0.2, 0.8);
        let transients = Transients::new(&samples, rate);

        assert_eq!(transients.attack_near(1.5, 0.015), None);
    }

    #[test]
    fn a_kick_hits_the_low_band_and_a_hat_does_not() {
        let rate = 44_100;
        let mut samples = silence(rate, 2.0);
        add_kick(&mut samples, rate, 0.5, 0.8);
        add_hat(&mut samples, rate, 1.5, 0.8);
        let transients = Transients::new(&samples, rate);

        assert!(transients.kick_at(0.5) > 10.0 * transients.kick_at(1.5));
    }
}
