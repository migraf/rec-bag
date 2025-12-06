//! Track diagnostic checks for identifying and fixing compatibility issues.
//!
//! This module provides an extensible framework for analyzing tracks and detecting
//! potential problems like CDJ compatibility issues or missing metadata.

use crate::Track;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use thiserror::Error;
use uuid::Uuid;

/// Errors that can occur during diagnostic operations.
#[derive(Debug, Error)]
pub enum DoctorError {
    #[error("Failed to read file: {0}")]
    FileRead(#[from] std::io::Error),
    #[error("Invalid WAV file format")]
    InvalidWavFormat,
    #[error("Fix not supported for this issue")]
    FixNotSupported,
    #[error("Track file not found: {0}")]
    TrackNotFound(String),
}

/// Severity level of a diagnosed issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    /// Informational - nice to fix but not critical
    Info,
    /// Warning - should be fixed for best compatibility
    Warning,
    /// Error - likely to cause problems
    Error,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

/// Represents a diagnosed issue with a track.
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnosis {
    /// Name of the check that found this issue
    pub check_name: String,
    /// ID of the affected track
    pub track_id: Uuid,
    /// Path to the track file
    pub track_path: std::path::PathBuf,
    /// Severity of the issue
    pub severity: Severity,
    /// Human-readable description of the issue
    pub message: String,
    /// Whether this issue can be automatically fixed
    pub fixable: bool,
}

/// Trait for implementing diagnostic checks.
///
/// Each check examines tracks for a specific type of issue.
/// Implement this trait to add new diagnostic capabilities.
pub trait DiagnosticCheck: Send + Sync {
    /// Unique name identifier for this check.
    fn name(&self) -> &str;

    /// Human-readable description of what this check looks for.
    fn description(&self) -> &str;

    /// Check a track for issues.
    /// Returns `Some(Diagnosis)` if an issue is found, `None` otherwise.
    fn check(&self, track: &Track) -> Option<Diagnosis>;

    /// Attempt to fix the issue for a track.
    /// Returns `Ok(())` if the fix was successful.
    fn fix(&self, track: &Track) -> Result<(), DoctorError>;

    /// Whether this check's issues can be automatically fixed.
    fn can_fix(&self) -> bool;
}

// ============================================================================
// WAV_EXTENSIBLE Check - CDJ Compatibility
// ============================================================================

/// Detects WAV files with WAV_EXTENSIBLE format headers that cause CDJ compatibility issues.
///
/// WAV_EXTENSIBLE (format code 0xFFFE) is an extension to the RIFF/WAV header that
/// some audio software uses, but it can cause playback issues on Pioneer CDJs and
/// other DJ equipment. This check detects such files and can patch them to use
/// standard PCM format (0x0001).
pub struct WavExtensibleCheck;

impl WavExtensibleCheck {
    /// WAV format code for WAVE_FORMAT_EXTENSIBLE
    const WAV_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    /// WAV format code for standard PCM
    const WAV_FORMAT_PCM: u16 = 0x0001;
    /// Offset to the format code in a WAV file (after RIFF header and fmt chunk ID)
    const FORMAT_CODE_OFFSET: u64 = 20;

    /// Check if a file is a WAV file by extension.
    fn is_wav_file(path: &Path) -> bool {
        path.extension()
            .map(|ext| ext.to_string_lossy().to_lowercase() == "wav")
            .unwrap_or(false)
    }

    /// Read the format code from a WAV file.
    fn read_format_code(path: &Path) -> Result<u16, DoctorError> {
        let mut file = File::open(path)?;

        // Verify RIFF header
        let mut riff_header = [0u8; 4];
        file.read_exact(&mut riff_header)?;
        if &riff_header != b"RIFF" {
            return Err(DoctorError::InvalidWavFormat);
        }

        // Skip file size (4 bytes)
        file.seek(SeekFrom::Current(4))?;

        // Verify WAVE format
        let mut wave_header = [0u8; 4];
        file.read_exact(&mut wave_header)?;
        if &wave_header != b"WAVE" {
            return Err(DoctorError::InvalidWavFormat);
        }

        // Find fmt chunk
        loop {
            let mut chunk_id = [0u8; 4];
            if file.read_exact(&mut chunk_id).is_err() {
                return Err(DoctorError::InvalidWavFormat);
            }

            let mut chunk_size = [0u8; 4];
            file.read_exact(&mut chunk_size)?;
            let size = u32::from_le_bytes(chunk_size);

            if &chunk_id == b"fmt " {
                // Read format code (first 2 bytes of fmt chunk)
                let mut format_code = [0u8; 2];
                file.read_exact(&mut format_code)?;
                return Ok(u16::from_le_bytes(format_code));
            }

            // Skip to next chunk
            file.seek(SeekFrom::Current(size as i64))?;
        }
    }

    /// Write a new format code to a WAV file.
    fn write_format_code(path: &Path, format_code: u16) -> Result<(), DoctorError> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;

        // Verify RIFF header
        let mut riff_header = [0u8; 4];
        file.read_exact(&mut riff_header)?;
        if &riff_header != b"RIFF" {
            return Err(DoctorError::InvalidWavFormat);
        }

        // Skip file size (4 bytes)
        file.seek(SeekFrom::Current(4))?;

        // Verify WAVE format
        let mut wave_header = [0u8; 4];
        file.read_exact(&mut wave_header)?;
        if &wave_header != b"WAVE" {
            return Err(DoctorError::InvalidWavFormat);
        }

        // Find fmt chunk and get its position
        loop {
            let mut chunk_id = [0u8; 4];
            if file.read_exact(&mut chunk_id).is_err() {
                return Err(DoctorError::InvalidWavFormat);
            }

            let mut chunk_size = [0u8; 4];
            file.read_exact(&mut chunk_size)?;
            let size = u32::from_le_bytes(chunk_size);

            if &chunk_id == b"fmt " {
                // We're now at the format code position - write the new value
                file.write_all(&format_code.to_le_bytes())?;
                file.flush()?;
                return Ok(());
            }

            // Skip to next chunk
            file.seek(SeekFrom::Current(size as i64))?;
        }
    }
}

impl DiagnosticCheck for WavExtensibleCheck {
    fn name(&self) -> &str {
        "wav_extensible"
    }

    fn description(&self) -> &str {
        "Detects WAV files with WAV_EXTENSIBLE headers that may not play on Pioneer CDJs"
    }

    fn check(&self, track: &Track) -> Option<Diagnosis> {
        // Only check WAV files
        if !Self::is_wav_file(&track.path) {
            return None;
        }

        // Check if file exists
        if !track.path.exists() {
            return None;
        }

        // Read format code
        match Self::read_format_code(&track.path) {
            Ok(format_code) if format_code == Self::WAV_FORMAT_EXTENSIBLE => Some(Diagnosis {
                check_name: self.name().to_string(),
                track_id: track.id,
                track_path: track.path.clone(),
                severity: Severity::Warning,
                message: "WAV file uses WAVE_FORMAT_EXTENSIBLE header which may not play on CDJs"
                    .to_string(),
                fixable: true,
            }),
            _ => None,
        }
    }

    fn fix(&self, track: &Track) -> Result<(), DoctorError> {
        if !track.path.exists() {
            return Err(DoctorError::TrackNotFound(
                track.path.display().to_string(),
            ));
        }

        // Verify it's actually a WAV_EXTENSIBLE file before fixing
        let format_code = Self::read_format_code(&track.path)?;
        if format_code != Self::WAV_FORMAT_EXTENSIBLE {
            return Ok(()); // Nothing to fix
        }

        // Patch the format code to standard PCM
        Self::write_format_code(&track.path, Self::WAV_FORMAT_PCM)
    }

    fn can_fix(&self) -> bool {
        true
    }
}

// ============================================================================
// Metadata Completeness Check
// ============================================================================

/// Detects tracks with missing or incomplete metadata.
///
/// Checks for:
/// - Missing artist
/// - Missing/default title (using filename)
/// - Missing BPM (important for DJ use)
/// - Missing genre
pub struct MetadataCheck;

impl MetadataCheck {
    /// Check if the title appears to be just the filename.
    fn is_filename_title(track: &Track) -> bool {
        if let Some(stem) = track.path.file_stem() {
            let stem_str = stem.to_string_lossy();
            // Title matches filename stem (case-insensitive)
            track.title.to_lowercase() == stem_str.to_lowercase()
        } else {
            false
        }
    }
}

impl DiagnosticCheck for MetadataCheck {
    fn name(&self) -> &str {
        "metadata_completeness"
    }

    fn description(&self) -> &str {
        "Identifies tracks with missing artist, title, BPM, or genre metadata"
    }

    fn check(&self, track: &Track) -> Option<Diagnosis> {
        let mut issues = Vec::new();

        // Check for missing artist
        if track.artist.is_none() || track.artist.as_ref().map(|a| a.is_empty()).unwrap_or(false) {
            issues.push("missing artist");
        }

        // Check for filename-as-title
        if Self::is_filename_title(track) {
            issues.push("title is filename");
        }

        // Check for missing BPM (important for DJ use)
        if track.bpm.is_none() {
            issues.push("missing BPM");
        }

        // Check for missing genre
        if track.genre.is_none() || track.genre.as_ref().map(|g| g.is_empty()).unwrap_or(false) {
            issues.push("missing genre");
        }

        if issues.is_empty() {
            None
        } else {
            // Determine severity based on what's missing
            let severity = if track.artist.is_none() || Self::is_filename_title(track) {
                Severity::Warning
            } else {
                Severity::Info
            };

            Some(Diagnosis {
                check_name: self.name().to_string(),
                track_id: track.id,
                track_path: track.path.clone(),
                severity,
                message: format!("Incomplete metadata: {}", issues.join(", ")),
                fixable: false,
            })
        }
    }

    fn fix(&self, _track: &Track) -> Result<(), DoctorError> {
        // Metadata issues require manual intervention
        Err(DoctorError::FixNotSupported)
    }

    fn can_fix(&self) -> bool {
        false
    }
}

// ============================================================================
// Doctor - Main diagnostic orchestrator
// ============================================================================

/// The Doctor orchestrates running diagnostic checks on tracks.
pub struct Doctor {
    checks: Vec<Box<dyn DiagnosticCheck>>,
}

impl Default for Doctor {
    fn default() -> Self {
        Self::new()
    }
}

impl Doctor {
    /// Create a new Doctor with all built-in checks.
    pub fn new() -> Self {
        Self {
            checks: vec![Box::new(WavExtensibleCheck), Box::new(MetadataCheck)],
        }
    }

    /// Create a Doctor with custom checks.
    pub fn with_checks(checks: Vec<Box<dyn DiagnosticCheck>>) -> Self {
        Self { checks }
    }

    /// Add a diagnostic check.
    pub fn add_check(&mut self, check: Box<dyn DiagnosticCheck>) {
        self.checks.push(check);
    }

    /// Get all registered checks.
    pub fn checks(&self) -> &[Box<dyn DiagnosticCheck>] {
        &self.checks
    }

    /// Run all checks on a single track.
    pub fn check_track(&self, track: &Track) -> Vec<Diagnosis> {
        self.checks.iter().filter_map(|c| c.check(track)).collect()
    }

    /// Run all checks on multiple tracks.
    pub fn check_tracks(&self, tracks: &[Track]) -> Vec<Diagnosis> {
        tracks
            .iter()
            .flat_map(|track| self.check_track(track))
            .collect()
    }

    /// Fix a specific diagnosis.
    pub fn fix_diagnosis(&self, diagnosis: &Diagnosis, track: &Track) -> Result<(), DoctorError> {
        for check in &self.checks {
            if check.name() == diagnosis.check_name {
                return check.fix(track);
            }
        }
        Err(DoctorError::FixNotSupported)
    }

    /// Fix all fixable diagnoses for a track.
    pub fn fix_all_for_track(&self, track: &Track) -> Vec<Result<(), DoctorError>> {
        let diagnoses = self.check_track(track);
        diagnoses
            .iter()
            .filter(|d| d.fixable)
            .map(|d| self.fix_diagnosis(d, track))
            .collect()
    }

    /// Get a summary of diagnoses grouped by check name.
    pub fn summarize(diagnoses: &[Diagnosis]) -> DiagnosisSummary {
        use std::collections::HashMap;

        let mut by_check: HashMap<String, Vec<&Diagnosis>> = HashMap::new();
        let mut fixable_count = 0;

        for diagnosis in diagnoses {
            by_check
                .entry(diagnosis.check_name.clone())
                .or_default()
                .push(diagnosis);
            if diagnosis.fixable {
                fixable_count += 1;
            }
        }

        let groups: Vec<DiagnosisGroup> = by_check
            .into_iter()
            .map(|(check_name, items)| {
                let fixable = items.iter().filter(|d| d.fixable).count();
                DiagnosisGroup {
                    check_name,
                    count: items.len(),
                    fixable_count: fixable,
                    severity: items.first().map(|d| d.severity).unwrap_or(Severity::Info),
                }
            })
            .collect();

        DiagnosisSummary {
            total: diagnoses.len(),
            fixable_count,
            groups,
        }
    }
}

/// Summary of diagnoses grouped by check type.
#[derive(Debug, Clone)]
pub struct DiagnosisSummary {
    /// Total number of issues found
    pub total: usize,
    /// Number of issues that can be auto-fixed
    pub fixable_count: usize,
    /// Issues grouped by check
    pub groups: Vec<DiagnosisGroup>,
}

/// A group of diagnoses from the same check.
#[derive(Debug, Clone)]
pub struct DiagnosisGroup {
    /// Name of the check
    pub check_name: String,
    /// Number of issues in this group
    pub count: usize,
    /// Number that can be fixed
    pub fixable_count: usize,
    /// Severity level
    pub severity: Severity,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn create_test_track(path: std::path::PathBuf) -> Track {
        Track {
            id: Uuid::new_v4(),
            path,
            title: "Test Track".to_string(),
            artist: Some("Test Artist".to_string()),
            album: None,
            duration: None,
            bpm: Some(128.0),
            key: None,
            genre: Some("Electronic".to_string()),
            rating: None,
            comment: None,
            added_at: chrono::Utc::now(),
            last_played: None,
            play_count: 0,
            sample_rate: None,
            bitrate: None,
            year: None,
            track_number: None,
            waveform: None,
        }
    }

    #[test]
    fn test_metadata_check_complete() {
        let track = create_test_track("/tmp/test.mp3".into());
        let check = MetadataCheck;
        assert!(check.check(&track).is_none());
    }

    #[test]
    fn test_metadata_check_missing_artist() {
        let mut track = create_test_track("/tmp/test.mp3".into());
        track.artist = None;
        let check = MetadataCheck;
        let result = check.check(&track);
        assert!(result.is_some());
        assert!(result.unwrap().message.contains("missing artist"));
    }

    #[test]
    fn test_metadata_check_missing_bpm() {
        let mut track = create_test_track("/tmp/test.mp3".into());
        track.bpm = None;
        let check = MetadataCheck;
        let result = check.check(&track);
        assert!(result.is_some());
        assert!(result.unwrap().message.contains("missing BPM"));
    }

    #[test]
    fn test_doctor_runs_all_checks() {
        let track = create_test_track("/tmp/nonexistent.mp3".into());
        let doctor = Doctor::new();
        // Should run without panicking even for non-existent files
        let _ = doctor.check_track(&track);
    }
}

