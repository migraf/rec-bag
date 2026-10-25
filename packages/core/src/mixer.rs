//! The mixer between the decks: each deck's channel strip (a trim and a three-band
//! isolator EQ) and the crossfader that blends deck one into deck two.
//!
//! Every control is a knob position from -1 (fully left) to 1 (fully right), with 0 at
//! twelve o'clock meaning "leave the sound alone". This module owns how a position
//! becomes a gain, so the interface and the audio engine can never disagree about it.

/// How far the trim reaches either side of unity, in decibels.
pub const TRIM_RANGE_DB: f32 = 12.0;
/// How far an EQ band can boost, in decibels.
pub const EQ_BOOST_DB: f32 = 6.0;
/// How far an EQ band cuts just before its kill position, in decibels.
pub const EQ_CUT_DB: f32 = 26.0;
/// Where the low band hands over to the mid band.
pub const LOW_MID_CROSSOVER_HZ: f32 = 250.0;
/// Where the mid band hands over to the high band.
pub const MID_HIGH_CROSSOVER_HZ: f32 = 3_000.0;

/// Where one deck's channel strip knobs sit.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ChannelMix {
    pub trim: f32,
    pub high: f32,
    pub mid: f32,
    pub low: f32,
}

impl ChannelMix {
    /// The same strip with every knob back at twelve o'clock.
    pub fn neutral() -> Self {
        Self::default()
    }
}

/// The trim a knob position asks for, in decibels.
pub fn trim_db(position: f32) -> f32 {
    position.clamp(-1.0, 1.0) * TRIM_RANGE_DB
}

/// The EQ gain a knob position asks for, in decibels; `None` when the band is killed.
///
/// The two halves of the knob are scaled differently, like a DJ mixer's: the left half
/// cuts deep and ends in a full kill, the right half boosts gently.
pub fn eq_db(position: f32) -> Option<f32> {
    let position = position.clamp(-1.0, 1.0);
    if position <= -1.0 {
        None
    } else if position < 0.0 {
        Some(position * EQ_CUT_DB)
    } else {
        Some(position * EQ_BOOST_DB)
    }
}

/// The linear gain an EQ knob position applies to its band.
pub fn eq_gain(position: f32) -> f32 {
    eq_db(position).map_or(0.0, db_to_gain)
}

/// The linear gain a trim knob position applies to its deck.
pub fn trim_gain(position: f32) -> f32 {
    db_to_gain(trim_db(position))
}

/// How loud the left and right decks play for a crossfader position.
///
/// The curve is dipless: in the middle both decks play at full level, so a centred
/// crossfader leaves the mix to the channel faders; each side fades out smoothly only
/// as the crossfader moves away from it.
pub fn crossfader_gains(position: f32) -> (f32, f32) {
    let position = position.clamp(-1.0, 1.0);
    let fade = |away: f32| (away.max(0.0) * std::f32::consts::FRAC_PI_2).cos();
    (fade(position), fade(-position))
}

/// Convert decibels to a linear amplitude factor.
pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(feature = "audio")]
pub use dsp::{ChannelControls, MixedSource};

#[cfg(feature = "audio")]
mod dsp {
    use super::*;
    use rodio::source::SeekError;
    use rodio::Source;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// A deck's channel strip as the audio thread reads it.
    ///
    /// The interface writes linear gains here; [`MixedSource`] reads them while it plays.
    /// Atomics keep the audio thread from ever waiting on the interface.
    #[derive(Debug)]
    pub struct ChannelControls {
        /// Trim times crossfader: everything that scales the deck as a whole.
        level: AtomicU32,
        low: AtomicU32,
        mid: AtomicU32,
        high: AtomicU32,
    }

    impl Default for ChannelControls {
        fn default() -> Self {
            Self {
                level: AtomicU32::new(1f32.to_bits()),
                low: AtomicU32::new(1f32.to_bits()),
                mid: AtomicU32::new(1f32.to_bits()),
                high: AtomicU32::new(1f32.to_bits()),
            }
        }
    }

    impl ChannelControls {
        /// Apply a channel strip and the crossfader's gain for this deck.
        pub fn set(&self, mix: ChannelMix, crossfader_gain: f32) {
            let store =
                |slot: &AtomicU32, value: f32| slot.store(value.to_bits(), Ordering::Relaxed);
            store(&self.level, trim_gain(mix.trim) * crossfader_gain);
            store(&self.low, eq_gain(mix.low));
            store(&self.mid, eq_gain(mix.mid));
            store(&self.high, eq_gain(mix.high));
        }

        fn gains(&self) -> Gains {
            let load = |slot: &AtomicU32| f32::from_bits(slot.load(Ordering::Relaxed));
            Gains {
                level: load(&self.level),
                low: load(&self.low),
                mid: load(&self.mid),
                high: load(&self.high),
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Gains {
        level: f32,
        low: f32,
        mid: f32,
        high: f32,
    }

    impl Gains {
        /// Move a step of the way towards `target`, so a turned knob never clicks.
        fn approach(&mut self, target: Gains, step: f32) {
            let towards = |from: &mut f32, to: f32| *from += (to - *from) * step;
            towards(&mut self.level, target.level);
            towards(&mut self.low, target.low);
            towards(&mut self.mid, target.mid);
            towards(&mut self.high, target.high);
        }
    }

    /// One second-order section, in transposed direct form II.
    #[derive(Debug, Clone, Copy, Default)]
    struct Biquad {
        b0: f32,
        b1: f32,
        b2: f32,
        a1: f32,
        a2: f32,
        z1: f32,
        z2: f32,
    }

    /// Q of a second-order Butterworth section. Two in series make a Linkwitz-Riley
    /// crossover, whose low and high halves sum back to a flat response.
    const BUTTERWORTH_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

    #[derive(Clone, Copy)]
    enum Pass {
        Low,
        High,
        All,
    }

    impl Biquad {
        /// The RBJ cookbook section of `pass` at `frequency`.
        fn new(pass: Pass, sample_rate: u32, frequency: f32) -> Self {
            // Keep the corner below Nyquist for very low sample rates.
            let frequency = frequency.min(sample_rate as f32 * 0.45);
            let w0 = std::f32::consts::TAU * frequency / sample_rate as f32;
            let (sin, cos) = w0.sin_cos();
            let alpha = sin / (2.0 * BUTTERWORTH_Q);
            let (b0, b1, b2) = match pass {
                Pass::Low => ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0),
                Pass::High => ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0),
                Pass::All => (1.0 - alpha, -2.0 * cos, 1.0 + alpha),
            };
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

        #[inline]
        fn process(&mut self, x: f32) -> f32 {
            let y = self.b0 * x + self.z1;
            self.z1 = self.b1 * x - self.a1 * y + self.z2;
            self.z2 = self.b2 * x - self.a2 * y;
            y
        }
    }

    /// A fourth-order Linkwitz-Riley filter: two Butterworth sections in series.
    #[derive(Debug, Clone, Copy)]
    struct LinkwitzRiley([Biquad; 2]);

    impl LinkwitzRiley {
        fn new(pass: Pass, sample_rate: u32, frequency: f32) -> Self {
            let section = Biquad::new(pass, sample_rate, frequency);
            Self([section, section])
        }

        #[inline]
        fn process(&mut self, x: f32) -> f32 {
            let x = self.0[0].process(x);
            self.0[1].process(x)
        }
    }

    /// One audio channel split into low, mid and high bands.
    #[derive(Debug, Clone, Copy)]
    struct BandSplit {
        low: LinkwitzRiley,
        above_low: LinkwitzRiley,
        mid: LinkwitzRiley,
        high: LinkwitzRiley,
        /// Gives the low band the phase the upper crossover gives the other two, so
        /// the three bands sum back to the input's level at every frequency.
        low_phase: Biquad,
    }

    impl BandSplit {
        fn new(sample_rate: u32) -> Self {
            Self {
                low: LinkwitzRiley::new(Pass::Low, sample_rate, LOW_MID_CROSSOVER_HZ),
                above_low: LinkwitzRiley::new(Pass::High, sample_rate, LOW_MID_CROSSOVER_HZ),
                mid: LinkwitzRiley::new(Pass::Low, sample_rate, MID_HIGH_CROSSOVER_HZ),
                high: LinkwitzRiley::new(Pass::High, sample_rate, MID_HIGH_CROSSOVER_HZ),
                low_phase: Biquad::new(Pass::All, sample_rate, MID_HIGH_CROSSOVER_HZ),
            }
        }

        #[inline]
        fn process(&mut self, x: f32, gains: &Gains) -> f32 {
            let low = self.low_phase.process(self.low.process(x));
            let above_low = self.above_low.process(x);
            let mid = self.mid.process(above_low);
            let high = self.high.process(above_low);
            gains.level * (gains.low * low + gains.mid * mid + gains.high * high)
        }
    }

    /// A DJ-style isolator EQ for interleaved audio: each band can be boosted, cut, or
    /// killed outright without colouring the others.
    #[derive(Debug, Clone)]
    struct Isolator {
        splits: Vec<BandSplit>,
        current: Gains,
        /// How far the gains move towards their target on each frame.
        smoothing: f32,
    }

    /// How long a gain change takes to mostly settle.
    const SMOOTHING_SECONDS: f32 = 0.01;

    impl Isolator {
        fn new(channels: u16, sample_rate: u32) -> Self {
            let sample_rate = sample_rate.max(1);
            Self {
                splits: vec![BandSplit::new(sample_rate); usize::from(channels.max(1))],
                current: ChannelControls::default().gains(),
                smoothing: 1.0 - (-1.0 / (SMOOTHING_SECONDS * sample_rate as f32)).exp(),
            }
        }

        fn channels(&self) -> usize {
            self.splits.len()
        }

        /// Start from `gains` rather than ramping up to them.
        fn settle(&mut self, gains: Gains) {
            self.current = gains;
        }

        /// Move the gains on by one frame towards `target`.
        #[inline]
        fn advance(&mut self, target: Gains) {
            self.current.approach(target, self.smoothing);
        }

        /// Process one interleaved sample of `channel`.
        #[inline]
        fn process(&mut self, channel: usize, x: f32) -> f32 {
            self.splits[channel].process(x, &self.current)
        }
    }

    /// A decoded track played through a deck's channel strip.
    pub struct MixedSource<S> {
        input: S,
        controls: std::sync::Arc<ChannelControls>,
        isolator: Isolator,
        /// Which interleaved channel the next sample belongs to.
        channel: usize,
        /// Samples left before the input may change its channel count or sample rate.
        frame_left: Option<usize>,
    }

    impl<S> MixedSource<S>
    where
        S: Source<Item = f32>,
    {
        pub fn new(input: S, controls: std::sync::Arc<ChannelControls>) -> Self {
            let mut isolator = Isolator::new(input.channels(), input.sample_rate());
            isolator.settle(controls.gains());
            let frame_left = input.current_frame_len();
            Self {
                input,
                controls,
                isolator,
                channel: 0,
                frame_left,
            }
        }

        /// Follow the input to a new frame, rebuilding the filters if its format changed.
        fn next_frame(&mut self) {
            let channels = usize::from(self.input.channels().max(1));
            if channels != self.isolator.channels() {
                let gains = self.isolator.current;
                self.isolator = Isolator::new(self.input.channels(), self.input.sample_rate());
                self.isolator.settle(gains);
                self.channel = 0;
            }
            self.frame_left = self.input.current_frame_len();
        }
    }

    impl<S> Iterator for MixedSource<S>
    where
        S: Source<Item = f32>,
    {
        type Item = f32;

        #[inline]
        fn next(&mut self) -> Option<f32> {
            if self.frame_left == Some(0) {
                self.next_frame();
            }
            let x = self.input.next()?;
            if let Some(left) = &mut self.frame_left {
                *left = left.saturating_sub(1);
            }
            if self.channel == 0 {
                self.isolator.advance(self.controls.gains());
            }
            let y = self.isolator.process(self.channel, x);
            self.channel = (self.channel + 1) % self.isolator.channels();
            Some(y)
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            self.input.size_hint()
        }
    }

    impl<S> Source for MixedSource<S>
    where
        S: Source<Item = f32>,
    {
        fn current_frame_len(&self) -> Option<usize> {
            self.input.current_frame_len()
        }

        fn channels(&self) -> u16 {
            self.input.channels()
        }

        fn sample_rate(&self) -> u32 {
            self.input.sample_rate()
        }

        fn total_duration(&self) -> Option<Duration> {
            self.input.total_duration()
        }

        fn try_seek(&mut self, position: Duration) -> Result<(), SeekError> {
            self.input.try_seek(position)?;
            // Filter memory from before the seek would ring into the new position.
            let gains = self.isolator.current;
            self.isolator = Isolator::new(self.input.channels(), self.input.sample_rate());
            self.isolator.settle(gains);
            self.channel = 0;
            self.frame_left = self.input.current_frame_len();
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use rodio::buffer::SamplesBuffer;
        use std::sync::Arc;

        const RATE: u32 = 44_100;

        fn sine(frequency: f32, seconds: f32) -> Vec<f32> {
            let samples = (RATE as f32 * seconds) as usize;
            (0..samples)
                .map(|n| (std::f32::consts::TAU * frequency * n as f32 / RATE as f32).sin())
                .collect()
        }

        /// The level of the second half of `samples`, after the filters have settled.
        fn rms_db(samples: &[f32]) -> f32 {
            let tail = &samples[samples.len() / 2..];
            let rms = (tail.iter().map(|x| x * x).sum::<f32>() / tail.len() as f32).sqrt();
            20.0 * (rms * std::f32::consts::SQRT_2).log10()
        }

        fn play(input: Vec<f32>, mix: ChannelMix) -> Vec<f32> {
            let controls = Arc::new(ChannelControls::default());
            controls.set(mix, 1.0);
            MixedSource::new(SamplesBuffer::new(1, RATE, input), controls).collect()
        }

        #[test]
        fn a_neutral_strip_leaves_every_frequency_at_its_level() {
            for frequency in [40.0, 250.0, 1_000.0, 3_000.0, 12_000.0] {
                let level = rms_db(&play(sine(frequency, 0.5), ChannelMix::neutral()));
                assert!(level.abs() < 0.1, "{frequency} Hz came out at {level} dB");
            }
        }

        #[test]
        fn a_killed_band_silences_its_frequencies_and_spares_the_others() {
            let no_low = ChannelMix {
                low: -1.0,
                ..ChannelMix::neutral()
            };
            assert!(rms_db(&play(sine(50.0, 0.5), no_low)) < -40.0);
            assert!(rms_db(&play(sine(1_000.0, 0.5), no_low)).abs() < 0.5);

            let no_high = ChannelMix {
                high: -1.0,
                ..ChannelMix::neutral()
            };
            assert!(rms_db(&play(sine(10_000.0, 0.5), no_high)) < -40.0);
            assert!(rms_db(&play(sine(100.0, 0.5), no_high)).abs() < 0.5);

            let no_mid = ChannelMix {
                mid: -1.0,
                ..ChannelMix::neutral()
            };
            assert!(rms_db(&play(sine(900.0, 0.5), no_mid)) < -20.0);
        }

        #[test]
        fn trim_scales_the_whole_deck() {
            let boosted = ChannelMix {
                trim: 0.5,
                ..ChannelMix::neutral()
            };
            let level = rms_db(&play(sine(1_000.0, 0.5), boosted));
            assert!((level - 6.0).abs() < 0.1, "trim came out at {level} dB");
        }

        #[test]
        fn interleaved_channels_are_filtered_independently() {
            // Left carries bass, right carries treble; killing the low band must only
            // silence the left.
            let (bass, treble) = (sine(50.0, 0.5), sine(10_000.0, 0.5));
            let stereo: Vec<f32> = bass
                .iter()
                .zip(&treble)
                .flat_map(|(l, r)| [*l, *r])
                .collect();
            let controls = Arc::new(ChannelControls::default());
            controls.set(
                ChannelMix {
                    low: -1.0,
                    ..ChannelMix::neutral()
                },
                1.0,
            );
            let out: Vec<f32> =
                MixedSource::new(SamplesBuffer::new(2, RATE, stereo), controls).collect();
            let left: Vec<f32> = out.iter().step_by(2).copied().collect();
            let right: Vec<f32> = out.iter().skip(1).step_by(2).copied().collect();
            assert!(rms_db(&left) < -40.0);
            assert!(rms_db(&right).abs() < 0.5);
        }

        #[test]
        fn a_knob_turned_while_playing_reaches_the_audio() {
            let controls = Arc::new(ChannelControls::default());
            let mut source = MixedSource::new(
                SamplesBuffer::new(1, RATE, sine(1_000.0, 1.0)),
                controls.clone(),
            );
            let before: Vec<f32> = source.by_ref().take(RATE as usize / 2).collect();
            controls.set(ChannelMix::neutral(), 0.0);
            let after: Vec<f32> = source.collect();
            assert!(rms_db(&before).abs() < 0.1);
            assert!(rms_db(&after) < -80.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twelve_o_clock_is_unity() {
        assert_eq!(trim_gain(0.0), 1.0);
        assert_eq!(eq_gain(0.0), 1.0);
    }

    #[test]
    fn an_eq_knob_boosts_gently_and_cuts_to_a_kill() {
        assert_eq!(eq_db(1.0), Some(EQ_BOOST_DB));
        assert_eq!(eq_db(-0.5), Some(-EQ_CUT_DB / 2.0));
        assert_eq!(eq_db(-1.0), None);
        assert_eq!(eq_gain(-1.0), 0.0);
    }

    #[test]
    fn a_centred_crossfader_plays_both_decks_fully() {
        assert_eq!(crossfader_gains(0.0), (1.0, 1.0));
        let (left, right) = crossfader_gains(-1.0);
        assert_eq!(left, 1.0);
        assert!(right.abs() < 1e-6);
        let (left, right) = crossfader_gains(1.0);
        assert!(left.abs() < 1e-6);
        assert_eq!(right, 1.0);
    }
}
