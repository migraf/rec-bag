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
// Duplicate Detection
// ============================================================================

/// Represents a group of duplicate tracks.
#[derive(Debug, Clone)]
pub struct DuplicateGroup {
    /// The common identifier for this group (e.g., normalized "artist - title")
    pub key: String,
    /// Track IDs in this duplicate group
    pub track_ids: Vec<Uuid>,
    /// Track paths for display
    pub track_paths: Vec<std::path::PathBuf>,
}

/// Result of duplicate detection across the library.
#[derive(Debug, Clone, Default)]
pub struct DuplicateReport {
    /// Groups of duplicate tracks
    pub groups: Vec<DuplicateGroup>,
    /// Total number of duplicate tracks (including originals)
    pub total_duplicates: usize,
}

impl DuplicateReport {
    /// Check if there are any duplicates.
    pub fn has_duplicates(&self) -> bool {
        !self.groups.is_empty()
    }

    /// Get total number of duplicate groups.
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Get count of extra tracks (total duplicates minus one per group for the "original")
    pub fn extra_count(&self) -> usize {
        self.groups.iter().map(|g| g.track_ids.len().saturating_sub(1)).sum()
    }
}

/// How a duplicate match was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateMatchType {
    /// Exact match on normalized artist + title
    Exact,
    /// Artist-Title pattern found in another track's title (e.g., "Artist - Title" in title field)
    ArtistTitleInTitle,
    /// Match after removing common suffixes like "(Original Mix)", "(Radio Edit)", etc.
    NormalizedSuffix,
    /// Fuzzy match - titles are very similar (low edit distance)
    Fuzzy,
}

impl DuplicateMatchType {
    pub fn description(&self) -> &'static str {
        match self {
            Self::Exact => "exact match",
            Self::ArtistTitleInTitle => "artist/title in filename",
            Self::NormalizedSuffix => "same track, different version",
            Self::Fuzzy => "similar title",
        }
    }
}

/// Detects duplicate tracks in the library using multiple smart strategies.
pub struct DuplicateCheck;

impl DuplicateCheck {
    /// Common suffixes/patterns to strip for normalized comparison
    const STRIP_PATTERNS: &'static [&'static str] = &[
        "(original mix)",
        "(original)",
        "(radio edit)",
        "(radio mix)",
        "(extended mix)",
        "(extended)",
        "(club mix)",
        "(club edit)",
        "(remix)",
        "(remaster)",
        "(remastered)",
        "(deluxe)",
        "(bonus track)",
        "(single version)",
        "(album version)",
        "(live)",
        "(acoustic)",
        "(instrumental)",
        "[original mix]",
        "[radio edit]",
        "[extended mix]",
        "[remastered]",
    ];

    /// Words to normalize (e.g., "feat." -> "ft")
    const FEATURING_PATTERNS: &'static [&'static str] = &[
        "featuring",
        "feat.",
        "feat",
        "ft.",
        "ft",
        "with",
    ];

    /// Basic normalization: lowercase and trim
    fn normalize_basic(s: &str) -> String {
        s.trim().to_lowercase()
    }

    /// Deep normalization: remove common suffixes and normalize featuring patterns
    fn normalize_deep(s: &str) -> String {
        let mut result = Self::normalize_basic(s);

        // Remove common suffixes
        for pattern in Self::STRIP_PATTERNS {
            result = result.replace(pattern, "");
        }

        // Normalize featuring patterns to a common form
        for pattern in Self::FEATURING_PATTERNS {
            if result.contains(pattern) {
                // Just remove the featuring part entirely for comparison
                if let Some(pos) = result.find(pattern) {
                    // Find the end of the featuring clause (usually ends at next major punctuation or end)
                    let rest = &result[pos..];
                    let end = rest.find(|c: char| c == ')' || c == ']' || c == '-')
                        .map(|p| pos + p)
                        .unwrap_or(result.len());
                    result = format!("{}{}", &result[..pos], &result[end..]);
                }
            }
        }

        // Remove extra whitespace and punctuation
        result = result
            .chars()
            .filter(|c| c.is_alphanumeric() || c.is_whitespace())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        result.trim().to_string()
    }

    /// Generate a comparison key from artist and title (basic normalization).
    fn make_key(artist: Option<&str>, title: &str) -> String {
        let normalized_artist = artist
            .map(|a| Self::normalize_basic(a))
            .unwrap_or_default();
        let normalized_title = Self::normalize_basic(title);

        if normalized_artist.is_empty() {
            normalized_title
        } else {
            format!("{} - {}", normalized_artist, normalized_title)
        }
    }

    /// Generate a deeply normalized key for fuzzy matching.
    fn make_deep_key(artist: Option<&str>, title: &str) -> String {
        let normalized_artist = artist
            .map(|a| Self::normalize_deep(a))
            .unwrap_or_default();
        let normalized_title = Self::normalize_deep(title);

        if normalized_artist.is_empty() {
            normalized_title
        } else {
            format!("{} - {}", normalized_artist, normalized_title)
        }
    }

    /// Try to extract artist and title from a combined string like "Artist - Title"
    fn parse_artist_title(s: &str) -> Option<(String, String)> {
        // Common separators: " - ", " – ", " — ", " / "
        let separators = [" - ", " – ", " — ", " / "];

        for sep in separators {
            if let Some(pos) = s.find(sep) {
                let artist = s[..pos].trim().to_string();
                let title = s[pos + sep.len()..].trim().to_string();
                if !artist.is_empty() && !title.is_empty() {
                    return Some((artist, title));
                }
            }
        }
        None
    }

    /// Calculate Levenshtein edit distance between two strings.
    fn edit_distance(a: &str, b: &str) -> usize {
        let a_chars: Vec<char> = a.chars().collect();
        let b_chars: Vec<char> = b.chars().collect();
        let a_len = a_chars.len();
        let b_len = b_chars.len();

        if a_len == 0 { return b_len; }
        if b_len == 0 { return a_len; }

        let mut matrix = vec![vec![0usize; b_len + 1]; a_len + 1];

        for i in 0..=a_len { matrix[i][0] = i; }
        for j in 0..=b_len { matrix[0][j] = j; }

        for i in 1..=a_len {
            for j in 1..=b_len {
                let cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };
                matrix[i][j] = (matrix[i - 1][j] + 1)
                    .min(matrix[i][j - 1] + 1)
                    .min(matrix[i - 1][j - 1] + cost);
            }
        }

        matrix[a_len][b_len]
    }

    /// Check if two strings are similar enough (fuzzy match).
    /// Returns true if edit distance is within threshold based on string length.
    fn is_fuzzy_match(a: &str, b: &str) -> bool {
        let a_norm = Self::normalize_deep(a);
        let b_norm = Self::normalize_deep(b);

        // Exact match after normalization
        if a_norm == b_norm {
            return true;
        }

        let max_len = a_norm.len().max(b_norm.len());
        if max_len == 0 {
            return false;
        }

        let distance = Self::edit_distance(&a_norm, &b_norm);

        // Allow ~15% difference, minimum 2 chars, maximum 5 chars
        let threshold = (max_len / 7).clamp(2, 5);
        distance <= threshold
    }

    /// Find duplicate tracks using multiple detection strategies.
    pub fn find_duplicates(tracks: &[Track]) -> DuplicateReport {
        use std::collections::{HashMap, HashSet};

        // Track which track IDs have been assigned to a group
        let mut assigned: HashSet<Uuid> = HashSet::new();
        let mut groups: Vec<DuplicateGroup> = Vec::new();

        // Strategy 1: Exact match on normalized artist + title
        let mut by_exact_key: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, track) in tracks.iter().enumerate() {
            let key = Self::make_key(track.artist.as_deref(), &track.title);
            by_exact_key.entry(key).or_default().push(idx);
        }

        for (key, indices) in by_exact_key.iter() {
            if indices.len() > 1 {
                let track_ids: Vec<Uuid> = indices.iter().map(|&i| tracks[i].id).collect();
                let track_paths: Vec<_> = indices.iter().map(|&i| tracks[i].path.clone()).collect();

                for &id in &track_ids {
                    assigned.insert(id);
                }

                groups.push(DuplicateGroup {
                    key: key.clone(),
                    track_ids,
                    track_paths,
                });
            }
        }

        // Strategy 2: Artist-Title pattern in title field
        // e.g., Track A: artist="", title="Daft Punk - Around The World"
        //       Track B: artist="Daft Punk", title="Around The World"
        for (idx_a, track_a) in tracks.iter().enumerate() {
            if assigned.contains(&track_a.id) {
                continue;
            }

            // Try to parse "Artist - Title" from track_a's title
            if let Some((parsed_artist, parsed_title)) = Self::parse_artist_title(&track_a.title) {
                let parsed_key = Self::make_key(Some(&parsed_artist), &parsed_title);

                for (idx_b, track_b) in tracks.iter().enumerate() {
                    if idx_a == idx_b || assigned.contains(&track_b.id) {
                        continue;
                    }

                    let key_b = Self::make_key(track_b.artist.as_deref(), &track_b.title);

                    if Self::normalize_basic(&parsed_key) == Self::normalize_basic(&key_b) {
                        // Found a match
                        assigned.insert(track_a.id);
                        assigned.insert(track_b.id);

                        groups.push(DuplicateGroup {
                            key: key_b,
                            track_ids: vec![track_a.id, track_b.id],
                            track_paths: vec![track_a.path.clone(), track_b.path.clone()],
                        });
                        break;
                    }
                }
            }
        }

        // Strategy 3: Normalized suffix removal (same track, different version)
        // Group by deeply normalized key
        let mut by_deep_key: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, track) in tracks.iter().enumerate() {
            if assigned.contains(&track.id) {
                continue;
            }
            let key = Self::make_deep_key(track.artist.as_deref(), &track.title);
            if !key.is_empty() {
                by_deep_key.entry(key).or_default().push(idx);
            }
        }

        for (key, indices) in by_deep_key.iter() {
            if indices.len() > 1 {
                // Verify they're not already exact matches (those were handled above)
                let track_ids: Vec<Uuid> = indices.iter()
                    .filter(|&&i| !assigned.contains(&tracks[i].id))
                    .map(|&i| tracks[i].id)
                    .collect();

                if track_ids.len() > 1 {
                    let track_paths: Vec<_> = indices.iter()
                        .filter(|&&i| !assigned.contains(&tracks[i].id))
                        .map(|&i| tracks[i].path.clone())
                        .collect();

                    for &id in &track_ids {
                        assigned.insert(id);
                    }

                    // Use original title of first track for the key display
                    let display_key = format!("{} (versions)", key);
                    groups.push(DuplicateGroup {
                        key: display_key,
                        track_ids,
                        track_paths,
                    });
                }
            }
        }

        // Strategy 4: Fuzzy matching for remaining tracks (same artist, similar title)
        let mut by_artist: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, track) in tracks.iter().enumerate() {
            if assigned.contains(&track.id) {
                continue;
            }
            if let Some(artist) = &track.artist {
                let artist_key = Self::normalize_deep(artist);
                if !artist_key.is_empty() {
                    by_artist.entry(artist_key).or_default().push(idx);
                }
            }
        }

        for (_artist, indices) in by_artist.iter() {
            if indices.len() < 2 {
                continue;
            }

            // Compare pairs within the same artist
            let mut fuzzy_groups: Vec<Vec<usize>> = Vec::new();

            for &idx_a in indices {
                if assigned.contains(&tracks[idx_a].id) {
                    continue;
                }

                let title_a = &tracks[idx_a].title;
                let mut group = vec![idx_a];

                for &idx_b in indices {
                    if idx_a >= idx_b || assigned.contains(&tracks[idx_b].id) {
                        continue;
                    }

                    let title_b = &tracks[idx_b].title;

                    if Self::is_fuzzy_match(title_a, title_b) {
                        group.push(idx_b);
                    }
                }

                if group.len() > 1 {
                    fuzzy_groups.push(group);
                }
            }

            // Add fuzzy groups
            for group_indices in fuzzy_groups {
                let track_ids: Vec<Uuid> = group_indices.iter()
                    .filter(|&&i| !assigned.contains(&tracks[i].id))
                    .map(|&i| tracks[i].id)
                    .collect();

                if track_ids.len() > 1 {
                    let track_paths: Vec<_> = group_indices.iter()
                        .filter(|&&i| !assigned.contains(&tracks[i].id))
                        .map(|&i| tracks[i].path.clone())
                        .collect();

                    for &id in &track_ids {
                        assigned.insert(id);
                    }

                    // Use first track's info for key
                    let first_idx = group_indices[0];
                    let key = format!(
                        "{} (similar)",
                        Self::make_key(tracks[first_idx].artist.as_deref(), &tracks[first_idx].title)
                    );

                    groups.push(DuplicateGroup {
                        key,
                        track_ids,
                        track_paths,
                    });
                }
            }
        }

        let total_duplicates: usize = groups.iter().map(|g| g.track_ids.len()).sum();

        DuplicateReport {
            groups,
            total_duplicates,
        }
    }

    /// Convert duplicate report to diagnoses for individual tracks.
    pub fn to_diagnoses(report: &DuplicateReport) -> Vec<Diagnosis> {
        let mut diagnoses = Vec::new();

        for group in &report.groups {
            let total_copies = group.track_ids.len();

            // Create a diagnosis for each track in the group
            // All messages include the key for consistent grouping in the UI
            for (track_id, track_path) in group.track_ids.iter().zip(&group.track_paths) {
                let message = format!(
                    "Duplicate of \"{}\" ({} total copies)",
                    group.key,
                    total_copies
                );

                diagnoses.push(Diagnosis {
                    check_name: "duplicates".to_string(),
                    track_id: *track_id,
                    track_path: track_path.clone(),
                    severity: Severity::Warning,
                    message,
                    fixable: false, // User must decide which to keep
                });
            }
        }

        diagnoses
    }
}

// ============================================================================
// Doctor - Main diagnostic orchestrator
// ============================================================================

/// The Doctor orchestrates running diagnostic checks on tracks.
#[derive(Clone)]
pub struct Doctor {
    checks: Vec<std::sync::Arc<dyn DiagnosticCheck>>,
    check_duplicates: bool,
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
            checks: vec![
                std::sync::Arc::new(WavExtensibleCheck),
                std::sync::Arc::new(MetadataCheck),
            ],
            check_duplicates: true,
        }
    }

    /// Create a Doctor with custom checks.
    pub fn with_checks(checks: Vec<std::sync::Arc<dyn DiagnosticCheck>>) -> Self {
        Self {
            checks,
            check_duplicates: true,
        }
    }

    /// Enable or disable duplicate checking.
    pub fn set_check_duplicates(&mut self, enabled: bool) {
        self.check_duplicates = enabled;
    }

    /// Check if duplicate checking is enabled.
    pub fn checks_duplicates(&self) -> bool {
        self.check_duplicates
    }

    /// Add a diagnostic check.
    pub fn add_check(&mut self, check: std::sync::Arc<dyn DiagnosticCheck>) {
        self.checks.push(check);
    }

    /// Get all registered checks.
    pub fn checks(&self) -> &[std::sync::Arc<dyn DiagnosticCheck>] {
        &self.checks
    }

    /// Run all checks on a single track.
    pub fn check_track(&self, track: &Track) -> Vec<Diagnosis> {
        self.checks.iter().filter_map(|c| c.check(track)).collect()
    }

    /// Run all checks on multiple tracks, including library-wide checks like duplicates.
    pub fn check_tracks(&self, tracks: &[Track]) -> Vec<Diagnosis> {
        let mut diagnoses: Vec<Diagnosis> = tracks
            .iter()
            .flat_map(|track| self.check_track(track))
            .collect();

        // Run library-wide duplicate detection
        if self.check_duplicates {
            let duplicate_report = DuplicateCheck::find_duplicates(tracks);
            diagnoses.extend(DuplicateCheck::to_diagnoses(&duplicate_report));
        }

        diagnoses
    }

    /// Find duplicates in the library (returns detailed report).
    pub fn find_duplicates(&self, tracks: &[Track]) -> DuplicateReport {
        DuplicateCheck::find_duplicates(tracks)
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
            freq_waveform: None,
            beat_grid: None,
        }
    }

    fn create_track_with_metadata(path: &str, artist: &str, title: &str) -> Track {
        Track {
            id: Uuid::new_v4(),
            path: path.into(),
            title: title.to_string(),
            artist: Some(artist.to_string()),
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
            freq_waveform: None,
            beat_grid: None,
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

    #[test]
    fn test_duplicate_detection_no_duplicates() {
        let tracks = vec![
            create_track_with_metadata("/music/track1.mp3", "Artist A", "Song One"),
            create_track_with_metadata("/music/track2.mp3", "Artist B", "Song Two"),
            create_track_with_metadata("/music/track3.mp3", "Artist C", "Song Three"),
        ];

        let report = DuplicateCheck::find_duplicates(&tracks);
        assert!(!report.has_duplicates());
        assert_eq!(report.group_count(), 0);
    }

    #[test]
    fn test_duplicate_detection_exact_match() {
        let tracks = vec![
            create_track_with_metadata("/music/track1.mp3", "Artist A", "Song One"),
            create_track_with_metadata("/music/track2.mp3", "Artist A", "Song One"),
        ];

        let report = DuplicateCheck::find_duplicates(&tracks);
        assert!(report.has_duplicates());
        assert_eq!(report.group_count(), 1);
        assert_eq!(report.groups[0].track_ids.len(), 2);
    }

    #[test]
    fn test_duplicate_detection_case_insensitive() {
        let tracks = vec![
            create_track_with_metadata("/music/track1.mp3", "Artist A", "Song One"),
            create_track_with_metadata("/music/track2.mp3", "ARTIST A", "song one"),
            create_track_with_metadata("/music/track3.mp3", "artist a", "SONG ONE"),
        ];

        let report = DuplicateCheck::find_duplicates(&tracks);
        assert!(report.has_duplicates());
        assert_eq!(report.group_count(), 1);
        assert_eq!(report.groups[0].track_ids.len(), 3);
    }

    #[test]
    fn test_duplicate_detection_whitespace_trimmed() {
        let tracks = vec![
            create_track_with_metadata("/music/track1.mp3", "Artist A", "Song One"),
            create_track_with_metadata("/music/track2.mp3", "  Artist A  ", "  Song One  "),
        ];

        let report = DuplicateCheck::find_duplicates(&tracks);
        assert!(report.has_duplicates());
        assert_eq!(report.group_count(), 1);
    }

    #[test]
    fn test_duplicate_detection_multiple_groups() {
        let tracks = vec![
            create_track_with_metadata("/music/a1.mp3", "Artist A", "Song One"),
            create_track_with_metadata("/music/a2.mp3", "Artist A", "Song One"),
            create_track_with_metadata("/music/b1.mp3", "Artist B", "Song Two"),
            create_track_with_metadata("/music/b2.mp3", "Artist B", "Song Two"),
            create_track_with_metadata("/music/c1.mp3", "Artist C", "Unique Song"),
        ];

        let report = DuplicateCheck::find_duplicates(&tracks);
        assert!(report.has_duplicates());
        assert_eq!(report.group_count(), 2);
        assert_eq!(report.extra_count(), 2); // 2 extra copies (one per group)
    }

    #[test]
    fn test_duplicate_to_diagnoses() {
        let tracks = vec![
            create_track_with_metadata("/music/track1.mp3", "Artist A", "Song One"),
            create_track_with_metadata("/music/track2.mp3", "Artist A", "Song One"),
        ];

        let report = DuplicateCheck::find_duplicates(&tracks);
        let diagnoses = DuplicateCheck::to_diagnoses(&report);

        assert_eq!(diagnoses.len(), 2);
        assert!(diagnoses.iter().all(|d| d.check_name == "duplicates"));
        assert!(diagnoses.iter().all(|d| d.severity == Severity::Warning));
        assert!(diagnoses.iter().all(|d| !d.fixable)); // Duplicates require manual intervention
        // All diagnoses should have consistent message format with the key
        assert!(diagnoses.iter().all(|d| d.message.contains("artist a - song one")));
    }

    #[test]
    fn test_doctor_includes_duplicates() {
        let tracks = vec![
            create_track_with_metadata("/music/track1.mp3", "Artist A", "Song One"),
            create_track_with_metadata("/music/track2.mp3", "Artist A", "Song One"),
        ];

        let doctor = Doctor::new();
        let diagnoses = doctor.check_tracks(&tracks);

        // Should include duplicate diagnoses
        let duplicate_diagnoses: Vec<_> = diagnoses
            .iter()
            .filter(|d| d.check_name == "duplicates")
            .collect();
        assert_eq!(duplicate_diagnoses.len(), 2);
    }

    #[test]
    fn test_doctor_disable_duplicate_check() {
        let tracks = vec![
            create_track_with_metadata("/music/track1.mp3", "Artist A", "Song One"),
            create_track_with_metadata("/music/track2.mp3", "Artist A", "Song One"),
        ];

        let mut doctor = Doctor::new();
        doctor.set_check_duplicates(false);
        let diagnoses = doctor.check_tracks(&tracks);

        // Should NOT include duplicate diagnoses
        let duplicate_diagnoses: Vec<_> = diagnoses
            .iter()
            .filter(|d| d.check_name == "duplicates")
            .collect();
        assert!(duplicate_diagnoses.is_empty());
    }

    // ==================== Smart Duplicate Detection Tests ====================

    #[test]
    fn test_duplicate_artist_title_in_title() {
        // Track B has "Artist - Title" pattern in title field, Track A has them separated
        let mut track_a = create_track_with_metadata("/music/proper.mp3", "Daft Punk", "Around The World");
        let mut track_b = create_test_track("/music/combined.mp3".into());
        track_b.artist = None;
        track_b.title = "Daft Punk - Around The World".to_string();

        let tracks = vec![track_a, track_b];
        let report = DuplicateCheck::find_duplicates(&tracks);

        assert!(report.has_duplicates(), "Should detect artist-title pattern in title");
        assert_eq!(report.group_count(), 1);
    }

    #[test]
    fn test_duplicate_normalized_suffix_original_mix() {
        let tracks = vec![
            create_track_with_metadata("/music/track1.mp3", "Artist A", "Song One"),
            create_track_with_metadata("/music/track2.mp3", "Artist A", "Song One (Original Mix)"),
        ];

        let report = DuplicateCheck::find_duplicates(&tracks);
        assert!(report.has_duplicates(), "Should match tracks ignoring (Original Mix)");
    }

    #[test]
    fn test_duplicate_normalized_suffix_radio_edit() {
        let tracks = vec![
            create_track_with_metadata("/music/track1.mp3", "Artist A", "Song One"),
            create_track_with_metadata("/music/track2.mp3", "Artist A", "Song One (Radio Edit)"),
        ];

        let report = DuplicateCheck::find_duplicates(&tracks);
        assert!(report.has_duplicates(), "Should match tracks ignoring (Radio Edit)");
    }

    #[test]
    fn test_duplicate_normalized_suffix_extended() {
        let tracks = vec![
            create_track_with_metadata("/music/track1.mp3", "Artist A", "Song One (Original Mix)"),
            create_track_with_metadata("/music/track2.mp3", "Artist A", "Song One (Extended Mix)"),
        ];

        let report = DuplicateCheck::find_duplicates(&tracks);
        assert!(report.has_duplicates(), "Should match different versions of the same track");
    }

    #[test]
    fn test_duplicate_fuzzy_match_typo() {
        let tracks = vec![
            create_track_with_metadata("/music/track1.mp3", "Artist A", "Around The World"),
            create_track_with_metadata("/music/track2.mp3", "Artist A", "Around The Wolrd"), // typo
        ];

        let report = DuplicateCheck::find_duplicates(&tracks);
        assert!(report.has_duplicates(), "Should fuzzy match tracks with typos");
    }

    #[test]
    fn test_duplicate_fuzzy_match_slight_difference() {
        let tracks = vec![
            create_track_with_metadata("/music/track1.mp3", "Artist A", "Song Title"),
            create_track_with_metadata("/music/track2.mp3", "Artist A", "Song Titles"), // extra 's'
        ];

        let report = DuplicateCheck::find_duplicates(&tracks);
        assert!(report.has_duplicates(), "Should fuzzy match tracks with slight differences");
    }

    #[test]
    fn test_duplicate_fuzzy_no_match_different_songs() {
        let tracks = vec![
            create_track_with_metadata("/music/track1.mp3", "Artist A", "Completely Different Song"),
            create_track_with_metadata("/music/track2.mp3", "Artist A", "Another Unique Track"),
        ];

        let report = DuplicateCheck::find_duplicates(&tracks);
        assert!(!report.has_duplicates(), "Should not match completely different songs");
    }

    #[test]
    fn test_edit_distance() {
        assert_eq!(DuplicateCheck::edit_distance("hello", "hello"), 0);
        assert_eq!(DuplicateCheck::edit_distance("hello", "hallo"), 1);
        assert_eq!(DuplicateCheck::edit_distance("hello", "help"), 2);
        assert_eq!(DuplicateCheck::edit_distance("", "hello"), 5);
        assert_eq!(DuplicateCheck::edit_distance("hello", ""), 5);
    }

    #[test]
    fn test_parse_artist_title() {
        assert_eq!(
            DuplicateCheck::parse_artist_title("Artist - Title"),
            Some(("Artist".to_string(), "Title".to_string()))
        );
        assert_eq!(
            DuplicateCheck::parse_artist_title("Daft Punk - Around The World"),
            Some(("Daft Punk".to_string(), "Around The World".to_string()))
        );
        assert_eq!(
            DuplicateCheck::parse_artist_title("No separator here"),
            None
        );
    }

    #[test]
    fn test_normalize_deep_removes_suffixes() {
        let result = DuplicateCheck::normalize_deep("Song Title (Original Mix)");
        assert!(!result.contains("original"));
        assert!(!result.contains("mix"));

        let result = DuplicateCheck::normalize_deep("Song Title (Radio Edit)");
        assert!(!result.contains("radio"));
        assert!(!result.contains("edit"));
    }
}

