//! Waveform extraction and visualization data.
//!
//! Extracts audio samples from files and creates downsampled waveform data
//! suitable for visualization in the UI.

use serde::{Deserialize, Serialize};
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use thiserror::Error;

/// Number of waveform points to generate per second of audio.
/// Higher values = more detail, larger data size.
const POINTS_PER_SECOND: usize = 100;

/// Errors that can occur during waveform extraction.
#[derive(Debug, Error)]
pub enum WaveformError {
    #[error("Failed to open audio file: {0}")]
    FileOpen(#[from] std::io::Error),
    #[error("Failed to probe audio format: {0}")]
    Probe(String),
    #[error("No audio track found")]
    NoTrack,
    #[error("Failed to create decoder: {0}")]
    Decoder(String),
    #[error("Failed to decode audio: {0}")]
    Decode(String),
}

/// Waveform data for visualization.
/// Contains normalized peak values (0.0 to 1.0) for rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WaveformData {
    /// Peak values for each segment (0.0 to 1.0)
    pub peaks: Vec<f32>,
    /// Duration of the audio in seconds
    pub duration_secs: f64,
    /// Sample rate of the source audio
    pub sample_rate: u32,
}

impl WaveformData {
    /// Create empty waveform data.
    pub fn empty() -> Self {
        Self {
            peaks: Vec::new(),
            duration_secs: 0.0,
            sample_rate: 0,
        }
    }

    /// Check if waveform data is empty.
    pub fn is_empty(&self) -> bool {
        self.peaks.is_empty()
    }

    /// Get the number of peaks.
    pub fn len(&self) -> usize {
        self.peaks.len()
    }

    /// Get a slice of peaks for a given time range.
    /// Returns peaks between start_pct and end_pct (0.0 to 1.0).
    pub fn peaks_in_range(&self, start_pct: f64, end_pct: f64) -> &[f32] {
        if self.peaks.is_empty() {
            return &[];
        }
        let start_idx = ((start_pct * self.peaks.len() as f64) as usize).min(self.peaks.len());
        let end_idx = ((end_pct * self.peaks.len() as f64) as usize).min(self.peaks.len());
        &self.peaks[start_idx..end_idx]
    }
}

/// Extract waveform data from an audio file.
///
/// This function reads the audio file, extracts samples, and computes
/// peak values for visualization. The result is a compact representation
/// suitable for rendering waveforms in the UI.
pub fn extract_waveform(path: impl AsRef<Path>) -> Result<WaveformData, WaveformError> {
    let path = path.as_ref();
    
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
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| WaveformError::Probe(e.to_string()))?;
    
    let mut format = probed.format;
    
    // Find the first audio track
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or(WaveformError::NoTrack)?;
    
    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);
    
    // Calculate samples per peak based on sample rate
    let samples_per_peak = sample_rate as usize / POINTS_PER_SECOND;
    
    // Create a decoder
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| WaveformError::Decoder(e.to_string()))?;
    
    // Collect all samples and compute peaks
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
    
    // Calculate duration
    let duration_secs = all_samples.len() as f64 / sample_rate as f64;
    
    // Compute peaks
    let mut peaks = Vec::new();
    for chunk in all_samples.chunks(samples_per_peak) {
        // Find the peak (max absolute value) in this chunk
        let peak = chunk
            .iter()
            .map(|s| s.abs())
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(0.0);
        peaks.push(peak);
    }
    
    // Normalize peaks to 0.0-1.0 range
    let max_peak = peaks
        .iter()
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .copied()
        .unwrap_or(1.0)
        .max(0.001); // Avoid division by zero
    
    for peak in &mut peaks {
        *peak /= max_peak;
    }
    
    Ok(WaveformData {
        peaks,
        duration_secs,
        sample_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_waveform() {
        let wf = WaveformData::empty();
        assert!(wf.is_empty());
        assert_eq!(wf.len(), 0);
    }

    #[test]
    fn test_peaks_in_range() {
        let wf = WaveformData {
            peaks: vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0],
            duration_secs: 1.0,
            sample_rate: 44100,
        };
        
        let range = wf.peaks_in_range(0.0, 0.5);
        assert_eq!(range.len(), 5);
        
        let range = wf.peaks_in_range(0.5, 1.0);
        assert_eq!(range.len(), 5);
    }
}

