//! Audio analysis module for waveform, BPM, key, and beat grid detection.
//!
//! This module provides a unified analysis pipeline that extracts multiple
//! features from audio files in a single pass.

mod beatgrid;
mod bpm;
mod fft;
mod freq_waveform;
mod key;
mod traits;
mod types;
mod waveform;

pub use beatgrid::BeatGridAnalyzer;
pub use bpm::BpmAnalyzer;
pub use fft::{BandEnergies, FftProcessor, FrequencyBands, WindowType};
pub use freq_waveform::FrequencyWaveformAnalyzer;
pub use key::KeyAnalyzer;
pub use traits::{Analyzer, AnalyzerOutput};
pub use types::{
    AnalysisError, AnalysisResult, BeatGridResult, BpmResult, FrequencyWaveform, KeyResult,
    MusicalKey,
};
pub use waveform::WaveformAnalyzer;

/// Maximum number of mono samples to decode. This caps memory usage at roughly
/// 400 MB of f32 samples (100 million samples ≈ ~38 minutes at 44.1 kHz).
/// Files longer than this are analyzed using only the first portion.
const MAX_DECODE_SAMPLES: usize = 100_000_000;

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Analyze an audio file and extract all features.
///
/// This function decodes the audio once and runs all analyzers (waveform, BPM, key, beat grid)
/// on the decoded samples.
pub fn analyze_track(path: impl AsRef<Path>) -> Result<AnalysisResult, AnalysisError> {
    let (samples, sample_rate) = decode_audio(path.as_ref())?;
    analyze_samples(samples, sample_rate)
}

/// Analyze audio already held in memory, such as a provider stream, without touching disk.
pub fn analyze_audio_bytes(bytes: Arc<[u8]>) -> Result<AnalysisResult, AnalysisError> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
    let (samples, sample_rate) = decode_stream(mss, Hint::new())?;
    analyze_samples(samples, sample_rate)
}

/// Run every analyzer over decoded mono samples.
fn analyze_samples(samples: Vec<f32>, sample_rate: u32) -> Result<AnalysisResult, AnalysisError> {
    if samples.is_empty() {
        return Err(AnalysisError::InsufficientData);
    }

    let duration_secs = samples.len() as f64 / sample_rate as f64;

    // Run analyzers
    let waveform_analyzer = WaveformAnalyzer::new();
    let freq_waveform_analyzer = FrequencyWaveformAnalyzer::new();
    let bpm_analyzer = BpmAnalyzer::new();
    let key_analyzer = KeyAnalyzer::new();
    let beat_grid_analyzer = BeatGridAnalyzer::new();

    // Extract simple waveform (for backward compatibility)
    let waveform_peaks = match waveform_analyzer.analyze(&samples, sample_rate) {
        Ok(AnalyzerOutput::Waveform { peaks }) => peaks,
        Ok(_) => Vec::new(),
        Err(e) => {
            eprintln!("Waveform analysis failed: {}", e);
            Vec::new()
        }
    };

    // Extract frequency-band waveform
    let freq_waveform = match freq_waveform_analyzer.analyze(&samples, sample_rate) {
        Ok(AnalyzerOutput::FrequencyWaveform { low, mid, high }) => {
            Some(FrequencyWaveform { low, mid, high })
        }
        Ok(_) => None,
        Err(e) => {
            eprintln!("Frequency waveform analysis failed: {}", e);
            None
        }
    };

    // Detect BPM
    let bpm = match bpm_analyzer.analyze(&samples, sample_rate) {
        Ok(AnalyzerOutput::Bpm { bpm, confidence }) => Some(BpmResult { bpm, confidence }),
        Ok(_) => None,
        Err(e) => {
            eprintln!("BPM detection failed: {}", e);
            None
        }
    };

    // Detect key
    let key = match key_analyzer.analyze(&samples, sample_rate) {
        Ok(AnalyzerOutput::Key { key, confidence }) => Some(KeyResult { key, confidence }),
        Ok(_) => None,
        Err(e) => {
            eprintln!("Key detection failed: {}", e);
            None
        }
    };

    // Generate beat grid (requires BPM)
    let beat_grid = if let Some(ref bpm_result) = bpm {
        match beat_grid_analyzer.analyze_with_bpm(&samples, sample_rate, bpm_result.bpm) {
            Ok(AnalyzerOutput::BeatGrid {
                first_beat_ms,
                beat_interval_ms,
                beats,
            }) => Some(BeatGridResult {
                first_beat_ms,
                beat_interval_ms,
                beats,
            }),
            Ok(_) => None,
            Err(e) => {
                eprintln!("Beat grid detection failed: {}", e);
                None
            }
        }
    } else {
        None
    };

    Ok(AnalysisResult {
        waveform_peaks,
        freq_waveform,
        beat_grid,
        duration_secs,
        sample_rate,
        bpm,
        key,
    })
}

/// Decode audio file to mono f32 samples.
fn decode_audio(path: &Path) -> Result<(Vec<f32>, u32), AnalysisError> {
    // Open the audio file
    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // Create a hint for the format
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    decode_stream(mss, hint)
}

/// Decode a media stream to mono f32 samples.
fn decode_stream(mss: MediaSourceStream, hint: Hint) -> Result<(Vec<f32>, u32), AnalysisError> {
    // Probe the format
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| AnalysisError::Probe(e.to_string()))?;

    let mut format = probed.format;

    // Find the first audio track
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or(AnalysisError::NoTrack)?;

    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);

    // Create a decoder
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| AnalysisError::Decoder(e.to_string()))?;

    // Collect all samples, capped at MAX_DECODE_SAMPLES to prevent OOM on very large files.
    let mut all_samples: Vec<f32> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    // Read all packets
    loop {
        if all_samples.len() >= MAX_DECODE_SAMPLES {
            break;
        }
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(_) => break,
        };

        // Skip packets from other tracks
        if packet.track_id() != track_id {
            continue;
        }

        // Decode the packet
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(_) => continue,
        };

        // Get the audio buffer
        if sample_buf.is_none() {
            let spec = *decoded.spec();
            let duration = decoded.capacity() as u64;
            sample_buf = Some(SampleBuffer::new(duration, spec));
        }

        if let Some(ref mut buf) = sample_buf {
            buf.copy_interleaved_ref(decoded);

            // Mix down to mono by averaging channels
            let samples = buf.samples();
            for chunk in samples.chunks(channels) {
                let mono: f32 = chunk.iter().sum::<f32>() / channels as f32;
                all_samples.push(mono);
            }
        }
    }

    Ok((all_samples, sample_rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four seconds of a 120 BPM click track as 16-bit mono WAV bytes.
    fn click_track_wav() -> Vec<u8> {
        const RATE: u32 = 44_100;
        let samples: Vec<i16> = (0..RATE * 4)
            .map(|n| {
                let since_beat = n % (RATE / 2);
                if since_beat < 400 {
                    (((n as f32) * 0.3).sin() * 20_000.0) as i16
                } else {
                    0
                }
            })
            .collect();
        let data_len = samples.len() as u32 * 2;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&RATE.to_le_bytes());
        wav.extend_from_slice(&(RATE * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    #[test]
    fn audio_in_memory_is_analyzed_like_a_file() {
        let result = analyze_audio_bytes(Arc::from(click_track_wav())).unwrap();

        assert_eq!(result.sample_rate, 44_100);
        assert!((result.duration_secs - 4.0).abs() < 0.01);
        assert!(!result.waveform_peaks.is_empty());
    }

    #[test]
    fn bytes_that_are_not_audio_are_refused() {
        assert!(analyze_audio_bytes(Arc::from(&b"not audio"[..])).is_err());
    }

    #[test]
    fn test_musical_key_display() {
        assert_eq!(MusicalKey::AMinor.to_notation(), "Am");
        assert_eq!(MusicalKey::CMajor.to_notation(), "C");
        assert_eq!(MusicalKey::GbMinor.to_notation(), "F#m");
    }

    #[test]
    fn test_musical_key_camelot() {
        assert_eq!(MusicalKey::AMinor.to_camelot(), "8A");
        assert_eq!(MusicalKey::CMajor.to_camelot(), "8B");
    }
}
