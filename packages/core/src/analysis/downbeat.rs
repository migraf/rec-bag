//! Which beat of the grid is beat 1 of a bar.
//!
//! Kicks fall on every beat, so they cannot tell beat 1 from beat 3. The
//! arrangement can: drops, breakdowns, new bass lines and fills land on bar
//! starts, and the big ones on phrase starts. Each beat gets a coarse spectral
//! profile; at every beat the profiles of the bars before are compared with the
//! bars after, at one-, two-, four- and eight-bar scales. Peaks of that change
//! vote for their position in the bar, and the position with the most votes is
//! beat 1. Each scale's votes are normalised, so a long crescendo cannot
//! outvote the bar-scale changes, nor the many small changes a big one.

use super::attack::Transients;
use super::beatgrid::Grid;
use super::fft::Stft;

const FRAME: usize = 2048;
const HOP: usize = 512;
/// Log-spaced bands from 40 Hz to 10 kHz.
const BANDS: usize = 12;
const LOW_HZ: f32 = 40.0;
const HIGH_HZ: f32 = 10_000.0;
/// Energy 60 dB below the loudest band's level over the track is silence.
const BAND_FLOOR: f32 = 1e-6;
/// Comparison windows either side of a beat, in beats.
const SCALES: [usize; 4] = [4, 8, 16, 32];
/// Fewest beats either side of a change that it is measured over.
const MIN_WIDTH: usize = 2;
/// A change peak must be the largest within this many beats either side.
const PEAK_RADIUS: usize = 2;
/// ...and this many times the scale's median change: in a steady loop, the
/// change from beat to beat is only spectral jitter, and its peaks are noise.
const PEAK_SIGNIFICANCE: f64 = 2.0;
/// The winning position must beat the runner-up by this factor to be trusted.
const CLEAR_WIN: f64 = 1.1;

/// Band energies of every STFT frame.
#[derive(Debug, Clone, Default)]
pub struct BandFrames {
    frames: Vec<[f32; BANDS]>,
    /// Frames per second.
    rate: f64,
    /// Seconds a frame hears either side of its centre.
    half_frame: f64,
}

/// Measure the band energies of mono samples.
pub fn band_frames(samples: &[f32], sample_rate: u32) -> BandFrames {
    let mut stft = Stft::new(FRAME, HOP);
    let bin_hz = stft.bin_hz(sample_rate);
    let last_bin = stft.bins() - 1;
    let mut edges = [0usize; BANDS + 1];
    for (b, edge) in edges.iter_mut().enumerate() {
        let hz = LOW_HZ * (HIGH_HZ / LOW_HZ).powf(b as f32 / BANDS as f32);
        *edge = ((hz / bin_hz).round() as usize).min(last_bin);
    }
    // Every band at least one bin wide, however coarse the bins.
    for b in 1..=BANDS {
        edges[b] = edges[b].max(edges[b - 1] + 1);
    }

    let mut frames = Vec::with_capacity(stft.frame_count(samples.len()));
    stft.for_each_frame(samples, |_, mags| {
        let mut bands = [0.0f32; BANDS];
        for (b, band) in bands.iter_mut().enumerate() {
            let bins = &mags[edges[b].min(last_bin)..edges[b + 1].min(last_bin + 1)];
            *band = bins.iter().map(|m| m * m).sum::<f32>() / bins.len().max(1) as f32;
        }
        frames.push(bands);
    });
    BandFrames {
        frames,
        rate: sample_rate as f64 / HOP as f64,
        half_frame: FRAME as f64 / 2.0 / sample_rate as f64,
    }
}

/// Which of the grid's first four beats starts a bar.
pub fn downbeat_phase(frames: &BandFrames, grid: &Grid, transients: &Transients, duration: f64) -> usize {
    let beats = grid.beat_count(duration);
    // A last beat cut short by the end of the file would read as a change.
    let whole_beats = grid.beat_count(duration - grid.period + 1e-9);
    let profiles = beat_profiles(frames, grid, whole_beats);
    let votes = novelty_votes(&profiles);

    let mut order = [0usize, 1, 2, 3];
    order.sort_by(|&a, &b| votes[b].total_cmp(&votes[a]));
    if votes[order[0]] > 0.0 && votes[order[0]] >= CLEAR_WIN * votes[order[1]] {
        order[0]
    } else {
        first_kick_phase(grid, transients, beats)
    }
}

/// Log of the mean band energy over each beat.
///
/// A frame hears half a frame either side of its centre, so the frames
/// measured for a beat are centred from half a frame before it to half a
/// frame before the next: they hear the beat's own attack, never the next
/// beat's. Each frame stands for one hop of time around its centre and counts
/// by how much of that hop falls inside that span; counting frames whole would
/// make a profile depend on how frames happen to line up with the grid, and
/// that alignment cycles every few beats in a way that reads as structure.
/// Energy is averaged before the log, so one quiet frame cannot swing a beat,
/// and floored far below the track's loudest band, so the last traces of
/// sound in a nearly empty band cannot either.
fn beat_profiles(frames: &BandFrames, grid: &Grid, beats: usize) -> Vec<[f32; BANDS]> {
    let hop = 1.0 / frames.rate;
    let mut level = [0.0f32; BANDS];
    for frame in &frames.frames {
        for (l, v) in level.iter_mut().zip(frame) {
            *l += v;
        }
    }
    let loudest = level.iter().copied().fold(0.0f32, f32::max) / frames.frames.len().max(1) as f32;
    let floor = BAND_FLOOR * loudest + 1e-10;
    (0..beats)
        .map(|k| {
            let from = grid.beat(k) - frames.half_frame;
            let to = from + grid.period;
            let first = ((from * frames.rate - 0.5).floor().max(0.0)) as usize;
            let last = ((to * frames.rate + 0.5).ceil() as usize).min(frames.frames.len());
            let mut mean = [0.0f32; BANDS];
            let mut covered = 0.0f32;
            for i in first..last {
                let centre = i as f64 * hop;
                let overlap = ((centre + hop / 2.0).min(to) - (centre - hop / 2.0).max(from)).max(0.0) as f32;
                for (m, v) in mean.iter_mut().zip(&frames.frames[i]) {
                    *m += overlap * v;
                }
                covered += overlap;
            }
            for m in &mut mean {
                *m = (*m / covered.max(f32::EPSILON) + floor).ln();
            }
            mean
        })
        .collect()
}

/// Votes for each position in the bar from peaks of change in the profiles.
fn novelty_votes(profiles: &[[f32; BANDS]]) -> [f64; 4] {
    // Prefix sums make the mean of any run of beats O(bands).
    let mut prefix = vec![[0.0f64; BANDS]; profiles.len() + 1];
    for (k, p) in profiles.iter().enumerate() {
        for b in 0..BANDS {
            prefix[k + 1][b] = prefix[k][b] + p[b] as f64;
        }
    }
    // Near the ends of the track the windows shrink to what is there, down to
    // two beats, so the drums coming in after a short intro still count. One
    // beat against one beat is too noisy to vote.
    let mean_change = |k: usize, width: usize| {
        (0..BANDS)
            .map(|b| {
                let before = (prefix[k][b] - prefix[k - width][b]) / width as f64;
                let after = (prefix[k + width][b] - prefix[k][b]) / width as f64;
                (after - before).powi(2)
            })
            .sum::<f64>()
            .sqrt()
    };

    let mut votes = [0.0f64; 4];
    for scale in SCALES {
        let novelty: Vec<(usize, f64)> = (MIN_WIDTH..=profiles.len().saturating_sub(MIN_WIDTH))
            .map(|k| (k, mean_change(k, scale.min(k).min(profiles.len() - k))))
            .collect();
        if novelty.is_empty() {
            continue;
        }
        let mut sorted: Vec<f64> = novelty.iter().map(|n| n.1).collect();
        sorted.sort_by(f64::total_cmp);
        let floor = PEAK_SIGNIFICANCE * sorted[sorted.len() / 2];

        let peaks: Vec<(usize, f64)> = novelty
            .iter()
            .enumerate()
            .filter(|&(i, &(_, n))| {
                let lo = i.saturating_sub(PEAK_RADIUS);
                let hi = (i + PEAK_RADIUS).min(novelty.len() - 1);
                n > floor && (lo..=hi).all(|j| j == i || novelty[j].1 < n)
            })
            .map(|(_, &p)| p)
            .collect();
        let total: f64 = peaks.iter().map(|p| p.1).sum();
        if total <= 0.0 {
            continue;
        }
        for (k, n) in peaks {
            votes[k % 4] += n / total;
        }
    }
    votes
}

/// The position of the first beat with a full kick: tracks usually start on
/// a bar, so it is the best guess when the arrangement does not say.
fn first_kick_phase(grid: &Grid, transients: &Transients, beats: usize) -> usize {
    let kicks: Vec<f32> = (0..beats).map(|k| transients.kick_at(grid.beat(k))).collect();
    let mut sorted = kicks.clone();
    sorted.sort_by(f32::total_cmp);
    let Some(&strong) = sorted.get(sorted.len() * 9 / 10) else {
        return 0;
    };
    kicks
        .iter()
        .position(|&k| k > 0.0 && k >= 0.5 * strong)
        .map_or(0, |k| k % 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::test_signals::{add_hat, add_kick, add_tone, beat_times, silence};

    const RATE: u32 = 44_100;

    fn grid(first_beat: f64, bpm: f64) -> Grid {
        Grid {
            period: 60.0 / bpm,
            first_beat,
            downbeat: 0,
        }
    }

    #[test]
    fn changes_in_the_arrangement_mark_beat_one() {
        let (bpm, first, secs) = (128.0, 0.1, 120.0);
        let period = 60.0 / bpm;
        let mut samples = silence(RATE, secs);
        let beats = beat_times(bpm, first, secs);
        for &t in &beats {
            add_kick(&mut samples, RATE, t, 0.6);
        }
        // Sections of four bars start on beat 2 (the third beat): each has its
        // own bass note, and every other one has hats.
        for (section, start) in (2..beats.len()).step_by(16).enumerate() {
            let from = beats[start];
            let to = (from + 16.0 * period).min(secs);
            let note = [55.0, 73.4, 61.7, 82.4][section % 4];
            add_tone(&mut samples, RATE, from, to, note, 0.2);
            if section % 2 == 1 {
                for k in 0..16 {
                    add_hat(&mut samples, RATE, from + (k as f64 + 0.5) * period, 0.3);
                }
            }
        }

        let frames = band_frames(&samples, RATE);
        let transients = Transients::new(&samples, RATE);
        assert_eq!(downbeat_phase(&frames, &grid(first, bpm), &transients, secs), 2);
    }

    #[test]
    fn the_drums_coming_in_mark_beat_one() {
        let (bpm, secs) = (128.0, 30.0);
        let period = 60.0 / bpm;
        let mut samples = silence(RATE, secs);
        // An intro of three silent beats, then a steady loop.
        for t in beat_times(bpm, 0.1 + 3.0 * period, secs) {
            add_kick(&mut samples, RATE, t, 0.6);
        }

        let frames = band_frames(&samples, RATE);
        let transients = Transients::new(&samples, RATE);
        assert_eq!(downbeat_phase(&frames, &grid(0.1, bpm), &transients, secs), 3);
    }

    #[test]
    fn a_steady_loop_has_no_bar_starts_to_vote_for() {
        let (bpm, secs) = (128.0, 30.0);
        let mut samples = silence(RATE, secs + 1.0);
        for t in beat_times(bpm, 0.1, secs + 1.0) {
            add_kick(&mut samples, RATE, t, 0.6);
        }
        let g = grid(0.1, bpm);
        let profiles = beat_profiles(&band_frames(&samples, RATE), &g, g.beat_count(secs));

        let votes = novelty_votes(&profiles);
        let mut sorted = votes;
        sorted.sort_by(f64::total_cmp);
        assert!(sorted[3] < CLEAR_WIN * sorted[2] || sorted[3] == 0.0, "{votes:?}");
    }
}
