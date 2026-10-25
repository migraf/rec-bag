//! Beat grid fitting: one constant tempo, placed on the front of the kicks.
//!
//! 1. A comb (a ruler of teeth one period apart) is slid over the onset
//!    envelope at many periods and phases; the placement that collects the most
//!    onset energy gives a period accurate to a fraction of an envelope frame
//!    and a rough phase.
//! 2. Near every tooth the kick's attack is looked up at millisecond
//!    resolution. A weighted straight line through the attacks, refitted
//!    without the ones furthest from it, is the grid. Beats with no attack
//!    (intros, breakdowns) simply do not vote.
//! 3. Hats and claps can be sharper than the kick, so a grid can lock onto the
//!    off-beat. Whichever of the beat and the half-beat carries more kick-band
//!    energy wins, and the grid moves half a beat if it has to.
//! 4. Most dance music is produced at a whole-number tempo. A fit within
//!    0.1 BPM of one is snapped to it when that grid sits on the attacks
//!    nearly as well as the measured one.

use super::attack::Transients;
use super::downbeat::{band_frames, downbeat_phase};
use super::onset::{onset_envelope, OnsetEnvelope};
use super::traits::{Analyzer, AnalyzerOutput};
use super::types::{AnalysisError, BeatGridResult};

/// Phases the comb tries per period.
const PHASES: usize = 48;
/// How far either side of a predicted beat its attack is looked for: wide
/// while the period is only known from the envelope, narrow once fitted.
const WIDE_REACH: f64 = 0.025;
const NARROW_REACH: f64 = 0.012;
/// Fewer attacks than this cannot place a line.
const MIN_ATTACKS: usize = 8;
/// Share of the attacks furthest from the first fit left out of the refit.
const OUTLIER_SHARE: f64 = 0.2;
/// Attacks further than this fraction of a beat from the line never count.
const MAX_RESIDUAL: f64 = 0.1;
/// How much more kick the half-beat must carry to move the grid onto it.
const HALF_BEAT_MARGIN: f32 = 1.25;
/// Fits this close to a whole number are tried at that number.
const SNAP_BPM: f64 = 0.1;
/// The whole-number grid must score this share of the measured one.
const SNAP_FIT: f64 = 0.9;
/// Attacks score by closeness to the line, reaching zero at this distance.
const SNAP_TOLERANCE_SECS: f64 = 0.010;

/// A constant-tempo beat grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    /// Seconds between beats.
    pub period: f64,
    /// Seconds into the track of the first beat at or after zero.
    pub first_beat: f64,
    /// Which of the first four beats starts a bar.
    pub downbeat: usize,
}

impl Grid {
    /// A grid of `period` through a beat at `anchor` seconds.
    fn through(period: f64, anchor: f64) -> Self {
        Self {
            period,
            first_beat: anchor.rem_euclid(period),
            downbeat: 0,
        }
    }

    pub fn bpm(&self) -> f64 {
        60.0 / self.period
    }

    /// Time of beat `k`, counting from the first beat at or after zero.
    pub fn beat(&self, k: usize) -> f64 {
        self.first_beat + k as f64 * self.period
    }

    /// Number of beats before `duration` seconds.
    pub fn beat_count(&self, duration: f64) -> usize {
        if duration <= self.first_beat {
            0
        } else {
            ((duration - self.first_beat) / self.period).ceil() as usize
        }
    }

    /// Beat times in seconds up to `duration`.
    pub fn beats(&self, duration: f64) -> Vec<f64> {
        (0..self.beat_count(duration)).map(|k| self.beat(k)).collect()
    }

    pub fn to_result(&self, duration: f64) -> BeatGridResult {
        BeatGridResult {
            first_beat_ms: self.first_beat * 1000.0,
            beat_interval_ms: self.period * 1000.0,
            beats: self.beats(duration).into_iter().map(|t| t * 1000.0).collect(),
            downbeat_index: self.downbeat,
        }
    }
}

/// Fit a constant grid to a track whose tempo is roughly `bpm`.
pub fn fit_grid(env: &OnsetEnvelope, transients: &Transients, bpm: f64, duration: f64) -> Option<Grid> {
    let (period, anchor) = comb(env, bpm)?;
    let mut grid = Grid::through(period, anchor);
    for reach in [WIDE_REACH, NARROW_REACH] {
        grid = refine_on_attacks(grid, transients, duration, reach).unwrap_or(grid);
    }
    if on_the_off_beat(&grid, transients, duration) {
        grid = Grid::through(grid.period, grid.first_beat + grid.period / 2.0);
        for reach in [WIDE_REACH, NARROW_REACH] {
            grid = refine_on_attacks(grid, transients, duration, reach).unwrap_or(grid);
        }
    }
    Some(snap_to_whole_bpm(grid, transients, duration))
}

/// The period and phase of the comb collecting the most onset energy, as
/// (seconds per beat, seconds of one beat).
fn comb(env: &OnsetEnvelope, bpm: f64) -> Option<(f64, f64)> {
    if env.is_empty() || bpm <= 0.0 {
        return None;
    }
    // Teeth fall between envelope values; smoothing keeps a peak from slipping
    // between two of them.
    let smooth = env.smoothed(3);
    let len = smooth.len() as f64;
    let score = |period: f64, phase: f64| {
        let (mut sum, mut teeth) = (0.0f64, 0usize);
        let mut x = phase;
        while x < len {
            sum += smooth.sample_at(x) as f64;
            teeth += 1;
            x += period;
        }
        sum / teeth.max(1) as f64
    };

    let nominal = env.rate * 60.0 / bpm;
    let mut best = (f64::MIN, nominal, 0.0);
    // Coarse: ±2% in 0.1% steps, every phase.
    for step in -20..=20 {
        let period = nominal * (1.0 + step as f64 * 0.001);
        for j in 0..PHASES {
            let phase = period * j as f64 / PHASES as f64;
            let s = score(period, phase);
            if s > best.0 {
                best = (s, period, phase);
            }
        }
    }
    // Fine: ±0.12% in 0.005% steps, phases near the coarse winner.
    let (_, coarse_period, coarse_phase) = best;
    for step in -24..=24 {
        let period = coarse_period * (1.0 + step as f64 * 0.00005);
        for j in -4..=4 {
            let phase = coarse_phase + period * j as f64 / (PHASES * 4) as f64;
            let s = score(period, phase.rem_euclid(period));
            if s > best.0 {
                best = (s, period, phase.rem_euclid(period));
            }
        }
    }
    let (_, period, phase) = best;
    Some((period / env.rate, env.time_of(phase)))
}

/// An attack found near beat `k`: (beat index, seconds, weight).
type Hit = (f64, f64, f64);

/// The attacks near each of the grid's beats.
fn hits(grid: &Grid, transients: &Transients, duration: f64, reach: f64) -> Vec<Hit> {
    (0..grid.beat_count(duration))
        .filter_map(|k| {
            let attack = transients.attack_near(grid.beat(k), reach)?;
            Some((k as f64, attack.secs, (attack.strength as f64).sqrt()))
        })
        .collect()
}

/// Weighted least-squares line `t = anchor + k * period` through hits.
fn fit_line(hits: &[Hit]) -> Option<(f64, f64)> {
    let total: f64 = hits.iter().map(|h| h.2).sum();
    if hits.len() < MIN_ATTACKS || total <= 0.0 {
        return None;
    }
    let k_mean = hits.iter().map(|h| h.2 * h.0).sum::<f64>() / total;
    let t_mean = hits.iter().map(|h| h.2 * h.1).sum::<f64>() / total;
    let (mut cov, mut var) = (0.0, 0.0);
    for &(k, t, w) in hits {
        cov += w * (k - k_mean) * (t - t_mean);
        var += w * (k - k_mean) * (k - k_mean);
    }
    if var <= 0.0 {
        return None;
    }
    let period = cov / var;
    Some((t_mean - period * k_mean, period))
}

/// Refit the grid through the attacks near its beats.
fn refine_on_attacks(grid: Grid, transients: &Transients, duration: f64, reach: f64) -> Option<Grid> {
    let mut hits = hits(&grid, transients, duration, reach);
    let (anchor, period) = fit_line(&hits)?;

    // Refit without the attacks furthest from the first line.
    let residual = |h: &Hit| (h.1 - anchor - h.0 * period).abs();
    hits.retain(|h| residual(h) <= MAX_RESIDUAL * period);
    hits.sort_by(|a, b| residual(a).total_cmp(&residual(b)));
    hits.truncate(((hits.len() as f64) * (1.0 - OUTLIER_SHARE)).ceil() as usize);
    let (anchor, period) = fit_line(&hits)?;

    // A line that wandered off the comb's tempo locked onto something else.
    if (period / grid.period - 1.0).abs() > 0.01 {
        return None;
    }
    Some(Grid::through(period, anchor))
}

/// Whether the half-beats carry clearly more kick than the beats.
fn on_the_off_beat(grid: &Grid, transients: &Transients, duration: f64) -> bool {
    let (mut on, mut off) = (0.0f32, 0.0f32);
    for k in 0..grid.beat_count(duration) {
        on += transients.kick_at(grid.beat(k));
        off += transients.kick_at(grid.beat(k) + grid.period / 2.0);
    }
    off > on * HALF_BEAT_MARGIN
}

/// Snap a fit close to a whole-number tempo onto it when the music agrees.
fn snap_to_whole_bpm(grid: Grid, transients: &Transients, duration: f64) -> Grid {
    let whole = grid.bpm().round();
    if (grid.bpm() - whole).abs() >= SNAP_BPM || whole <= 0.0 {
        return grid;
    }
    let mut hits = hits(&grid, transients, duration, NARROW_REACH);
    let anchor = grid.first_beat;
    hits.retain(|h| (h.1 - anchor - h.0 * grid.period).abs() <= MAX_RESIDUAL * grid.period);
    let total: f64 = hits.iter().map(|h| h.2).sum();
    if hits.len() < MIN_ATTACKS || total <= 0.0 {
        return grid;
    }

    let period = 60.0 / whole;
    let whole_anchor = hits.iter().map(|h| h.2 * (h.1 - h.0 * period)).sum::<f64>() / total;
    let fit = |anchor: f64, period: f64| {
        hits.iter()
            .map(|h| h.2 * (1.0 - (h.1 - anchor - h.0 * period).abs() / SNAP_TOLERANCE_SECS).max(0.0))
            .sum::<f64>()
    };
    if fit(whole_anchor, period) >= SNAP_FIT * fit(anchor, grid.period) {
        Grid::through(period, whole_anchor)
    } else {
        grid
    }
}

/// Beat grid analyzer: fits a grid, then finds which beat starts a bar.
pub struct BeatGridAnalyzer;

impl Default for BeatGridAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl BeatGridAnalyzer {
    pub fn new() -> Self {
        Self
    }

    /// Fit the grid for a track whose tempo is roughly `bpm`.
    pub fn analyze_with_bpm(
        &self,
        samples: &[f32],
        sample_rate: u32,
        bpm: f32,
    ) -> Result<AnalyzerOutput, AnalysisError> {
        let duration = samples.len() as f64 / sample_rate as f64;
        let env = onset_envelope(samples, sample_rate);
        let transients = Transients::new(samples, sample_rate);
        let mut grid = fit_grid(&env, &transients, bpm as f64, duration)
            .ok_or(AnalysisError::InsufficientData)?;
        grid.downbeat = downbeat_phase(&band_frames(samples, sample_rate), &grid, &transients, duration);
        let result = grid.to_result(duration);
        Ok(AnalyzerOutput::BeatGrid {
            first_beat_ms: result.first_beat_ms,
            beat_interval_ms: result.beat_interval_ms,
            beats: result.beats,
            downbeat_index: result.downbeat_index,
        })
    }
}

impl Analyzer for BeatGridAnalyzer {
    fn name(&self) -> &'static str {
        "BeatGrid"
    }

    fn analyze(&self, samples: &[f32], sample_rate: u32) -> Result<AnalyzerOutput, AnalysisError> {
        let env = onset_envelope(samples, sample_rate);
        let bpm = super::bpm::BpmAnalyzer::new()
            .estimate(&env)
            .ok_or(AnalysisError::InsufficientData)?;
        self.analyze_with_bpm(samples, sample_rate, bpm.bpm as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::test_signals::{add_hat, add_kick, beat_times, click_track, silence};

    const RATE: u32 = 44_100;

    fn fit(samples: &[f32], bpm_hint: f64) -> Grid {
        let env = onset_envelope(samples, RATE);
        let transients = Transients::new(samples, RATE);
        let duration = samples.len() as f64 / RATE as f64;
        fit_grid(&env, &transients, bpm_hint, duration).unwrap()
    }

    #[test]
    fn grid_lands_on_the_kicks() {
        let samples = click_track(RATE, 128.0, 0.137, 60.0);
        // A hint off by a few tenths, as the tempo estimate can be.
        let grid = fit(&samples, 128.3);

        assert_eq!(grid.bpm(), 128.0);
        assert!((grid.first_beat - 0.137).abs() < 0.002, "{}", grid.first_beat);
    }

    #[test]
    fn fractional_tempos_are_kept() {
        let samples = click_track(RATE, 127.5, 0.3, 60.0);
        let grid = fit(&samples, 127.2);

        assert!((grid.bpm() - 127.5).abs() < 0.01, "{}", grid.bpm());
        let last = grid.beat(grid.beat_count(60.0) - 1);
        let expected = *beat_times(127.5, 0.3, 60.0).last().unwrap();
        assert!((last - expected).abs() < 0.003, "{last} vs {expected}");
    }

    #[test]
    fn loud_off_beat_hats_do_not_pull_the_grid_off_the_kick() {
        let mut samples = silence(RATE, 60.0);
        let half = 60.0 / 128.0 / 2.0;
        for t in beat_times(128.0, 0.2, 60.0) {
            add_kick(&mut samples, RATE, t, 0.4);
            add_hat(&mut samples, RATE, t + half, 1.0);
        }
        let grid = fit(&samples, 128.0);

        let offset = (grid.first_beat - 0.2).rem_euclid(grid.period);
        let offset = offset.min(grid.period - offset);
        assert!(offset < 0.003, "grid is {offset}s from the kicks");
    }

    #[test]
    fn beats_start_at_the_first_beat_and_stop_before_the_end() {
        let grid = Grid::through(0.5, 0.25);
        let beats = grid.beats(2.0);
        assert_eq!(beats, vec![0.25, 0.75, 1.25, 1.75]);
    }
}
