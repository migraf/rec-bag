//! Audio analysis module for waveform, BPM, and key detection.
//!
//! This module provides a unified analysis pipeline that extracts multiple
//! features from audio files in a single pass.

mod bpm;
mod key;
mod traits;
mod types;
mod waveform;

pub use bpm::BpmAnalyzer;
pub use key::KeyAnalyzer;
pub use traits::{Analyzer, AnalyzerOutput};
pub use types::{AnalysisError, AnalysisResult, BpmResult, KeyResult, MusicalKey};
pub use waveform::WaveformAnalyzer;

use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Analyze an audio file and extract all features.
///
/// This function decodes the audio once and runs all analyzers (waveform, BPM, key)
/// on the decoded samples.
pub fn analyze_track(path: impl AsRef<Path>) -> Result<AnalysisResult, AnalysisError> {
    let path = path.as_ref();

    // Decode audio to samples
    let (samples, sample_rate) = decode_audio(path)?;

    if samples.is_empty() {
        return Err(AnalysisError::InsufficientData);
    }

    let duration_secs = samples.len() as f64 / sample_rate as f64;

    // Run analyzers
    let waveform_analyzer = WaveformAnalyzer::new();
    let bpm_analyzer = BpmAnalyzer::new();
    let key_analyzer = KeyAnalyzer::new();

    // Extract waveform
    let waveform_peaks = match waveform_analyzer.analyze(&samples, sample_rate) {
        Ok(AnalyzerOutput::Waveform { peaks }) => peaks,
        Ok(_) => Vec::new(),
        Err(e) => {
            eprintln!("Waveform analysis failed: {}", e);
            Vec::new()
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

    Ok(AnalysisResult {
        waveform_peaks,
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

    // Collect all samples
    let mut all_samples: Vec<f32> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    // Read all packets
    loop {
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

