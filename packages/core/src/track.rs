//! Track model representing an audio file with metadata.

use chrono::{DateTime, Utc};
use lofty::prelude::*;
use lofty::probe::Probe;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

/// Waveform peaks data for visualization (stored as JSON blob).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct WaveformPeaks {
    /// Normalized peak values (0.0 to 1.0)
    pub peaks: Vec<f32>,
}

impl WaveformPeaks {
    /// Create empty waveform peaks.
    pub fn empty() -> Self {
        Self { peaks: Vec::new() }
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.peaks.is_empty()
    }
}

/// Frequency-band waveform data for colored visualization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FrequencyWaveformData {
    /// Low frequency (bass) peaks.
    pub low: Vec<f32>,
    /// Mid frequency peaks.
    pub mid: Vec<f32>,
    /// High frequency (treble) peaks.
    pub high: Vec<f32>,
}

impl FrequencyWaveformData {
    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.low.is_empty() && self.mid.is_empty() && self.high.is_empty()
    }

    /// Get length (number of points).
    pub fn len(&self) -> usize {
        self.low.len()
    }
}

/// Beat grid data for visualization and synchronization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct BeatGridData {
    /// Time of the first beat in milliseconds.
    pub first_beat_ms: f64,
    /// Time between beats in milliseconds (derived from BPM).
    pub beat_interval_ms: f64,
    /// All beat positions in milliseconds.
    pub beats: Vec<f64>,
    /// Index in `beats` of the first downbeat; bars start every four beats from it.
    #[serde(default)]
    pub downbeat_index: usize,
}

impl BeatGridData {
    /// Check if the beat grid is empty.
    pub fn is_empty(&self) -> bool {
        self.beats.is_empty()
    }

    /// Check if a beat is the start of a bar (every 4th beat from the downbeat).
    pub fn is_bar_start(&self, beat_index: usize) -> bool {
        beat_index % 4 == self.downbeat_index % 4
    }
}

/// Represents a single audio track with its metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Track {
    /// Unique identifier for the track
    pub id: Uuid,
    /// File path to the audio file
    pub path: PathBuf,
    /// Track title (from metadata or filename)
    pub title: String,
    /// Artist name
    pub artist: Option<String>,
    /// Album name
    pub album: Option<String>,
    /// Track duration
    pub duration: Option<Duration>,
    /// Beats per minute (for DJ mixing)
    pub bpm: Option<f32>,
    /// Musical key (e.g., "Am", "C#m")
    pub key: Option<String>,
    /// Genre
    pub genre: Option<String>,
    /// User-defined rating (0-5)
    pub rating: Option<u8>,
    /// User comments/notes
    pub comment: Option<String>,
    /// When the track was added to the library
    pub added_at: DateTime<Utc>,
    /// Last time the track was played
    pub last_played: Option<DateTime<Utc>>,
    /// Number of times played
    pub play_count: u32,
    /// Sample rate (Hz)
    pub sample_rate: Option<u32>,
    /// Bitrate (kbps)
    pub bitrate: Option<u32>,
    /// Year/date of release
    pub year: Option<u32>,
    /// Track number in album
    pub track_number: Option<u32>,
    /// Waveform peaks for visualization (simple amplitude-based)
    #[serde(default)]
    pub waveform: Option<WaveformPeaks>,
    /// Frequency-band waveform for colored visualization
    #[serde(default)]
    pub freq_waveform: Option<FrequencyWaveformData>,
    /// Beat grid for beat markers and synchronization
    #[serde(default)]
    pub beat_grid: Option<BeatGridData>,
}

impl Track {
    /// Create a new track from a file path with automatic metadata extraction.
    pub fn new(path: PathBuf) -> Self {
        let mut track = Self::new_without_metadata(path.clone());
        track.load_metadata();
        track
    }

    /// Create a new track without reading metadata (faster, for bulk operations).
    pub fn new_without_metadata(path: PathBuf) -> Self {
        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string();

        Self {
            id: Uuid::new_v4(),
            path,
            title,
            artist: None,
            album: None,
            duration: None,
            bpm: None,
            key: None,
            genre: None,
            rating: None,
            comment: None,
            added_at: Utc::now(),
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

    /// Set waveform data from extracted peaks.
    pub fn set_waveform(&mut self, peaks: Vec<f32>) {
        self.waveform = Some(WaveformPeaks { peaks });
    }

    /// Set frequency waveform data.
    pub fn set_freq_waveform(&mut self, low: Vec<f32>, mid: Vec<f32>, high: Vec<f32>) {
        self.freq_waveform = Some(FrequencyWaveformData { low, mid, high });
    }

    /// Set beat grid data.
    pub fn set_beat_grid(&mut self, first_beat_ms: f64, beat_interval_ms: f64, beats: Vec<f64>) {
        self.beat_grid = Some(BeatGridData {
            first_beat_ms,
            beat_interval_ms,
            beats,
            downbeat_index: 0,
        });
    }

    /// Check if waveform data is available.
    pub fn has_waveform(&self) -> bool {
        self.waveform
            .as_ref()
            .map(|w| !w.is_empty())
            .unwrap_or(false)
    }

    /// Check if frequency waveform data is available.
    pub fn has_freq_waveform(&self) -> bool {
        self.freq_waveform
            .as_ref()
            .map(|w| !w.is_empty())
            .unwrap_or(false)
    }

    /// Check if beat grid is available.
    pub fn has_beat_grid(&self) -> bool {
        self.beat_grid
            .as_ref()
            .map(|b| !b.is_empty())
            .unwrap_or(false)
    }

    /// Check if BPM has been detected.
    pub fn has_bpm(&self) -> bool {
        self.bpm.is_some()
    }

    /// Check if musical key has been detected.
    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }

    /// Check if the track needs analysis (missing waveform, BPM, key, or beat grid).
    pub fn needs_analysis(&self) -> bool {
        !self.has_waveform()
            || !self.has_freq_waveform()
            || !self.has_bpm()
            || !self.has_key()
            || !self.has_beat_grid()
    }

    /// Load metadata from the audio file.
    pub fn load_metadata(&mut self) {
        if let Err(e) = self.try_load_metadata() {
            // Log error but don't fail - metadata is optional
            eprintln!("Failed to load metadata for {:?}: {}", self.path, e);
        }
    }

    /// Try to load metadata, returning an error if it fails.
    fn try_load_metadata(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let tags = read_embedded_tags(&self.path)?;
        self.sample_rate = tags.sample_rate;
        self.bitrate = tags.bitrate;
        self.duration = tags.duration.or(self.duration);
        if let Some(title) = tags.title {
            self.title = title;
        }
        self.artist = tags.artist.or(self.artist.take());
        self.album = tags.album.or(self.album.take());
        self.genre = tags.genre.or(self.genre.take());
        self.year = tags.year.or(self.year);
        self.track_number = tags.track_number.or(self.track_number);
        self.comment = tags.comment.or(self.comment.take());
        self.bpm = tags.bpm.or(self.bpm);
        self.key = tags.musical_key.or(self.key.take());
        Ok(())
    }

    /// Create a track with title and artist (manual override).
    pub fn with_metadata(path: PathBuf, title: String, artist: Option<String>) -> Self {
        let mut track = Self::new(path);
        track.title = title;
        track.artist = artist;
        track
    }

    /// Set the BPM value.
    pub fn set_bpm(&mut self, bpm: f32) {
        self.bpm = Some(bpm);
    }

    /// Set the musical key.
    pub fn set_key(&mut self, key: String) {
        self.key = Some(key);
    }

    /// Set the duration.
    pub fn set_duration(&mut self, duration: Duration) {
        self.duration = Some(duration);
    }

    /// Record that the track was played.
    pub fn record_play(&mut self) {
        self.last_played = Some(Utc::now());
        self.play_count += 1;
    }

    /// Set the rating (clamped to 0-5).
    pub fn set_rating(&mut self, rating: u8) {
        self.rating = Some(rating.min(5));
    }

    /// Get a formatted duration string (MM:SS).
    pub fn duration_string(&self) -> String {
        match self.duration {
            Some(d) => {
                let total_secs = d.as_secs();
                let mins = total_secs / 60;
                let secs = total_secs % 60;
                format!("{:02}:{:02}", mins, secs)
            }
            None => "--:--".to_string(),
        }
    }

    /// Get a formatted bitrate string.
    pub fn bitrate_string(&self) -> String {
        match self.bitrate {
            Some(b) => format!("{}kbps", b),
            None => "".to_string(),
        }
    }
}

/// Analyze multiple tracks in parallel (for bulk imports).
pub fn analyze_tracks(paths: Vec<PathBuf>) -> Vec<Track> {
    paths.into_iter().map(Track::new).collect()
}

/// What one audio file's own tags and audio properties say about it.
///
/// These are observations read off a single file, not a track's identity: a scan turns
/// them into metadata candidates and reconciliation decides which ones win. A field is
/// `None` when the file does not carry it, never a placeholder.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EmbeddedTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub comment: Option<String>,
    /// How long the audio runs, read from the file's properties rather than a tag.
    pub duration: Option<Duration>,
    pub year: Option<u32>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub bpm: Option<f32>,
    pub musical_key: Option<String>,
    pub isrc: Option<String>,
    pub sample_rate: Option<u32>,
    pub bitrate: Option<u32>,
}

/// Read every tag embedded in `path`, plus the audio properties of the file itself.
///
/// This is the one place rec_bag reads a file's own metadata. Tempo and key are stored
/// under different names by different software, so each is tried in turn rather than
/// trusting a single tag format.
pub fn read_embedded_tags(path: &Path) -> Result<EmbeddedTags, MetadataError> {
    let tagged_file = Probe::open(path)
        .map_err(|error| MetadataError::FileReadError(error.to_string()))?
        .read()
        .map_err(|error| MetadataError::FileReadError(error.to_string()))?;

    let properties = tagged_file.properties();
    let mut tags = EmbeddedTags {
        duration: Some(properties.duration()),
        sample_rate: properties.sample_rate(),
        bitrate: properties.audio_bitrate(),
        ..EmbeddedTags::default()
    };

    let Some(tag) = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
    else {
        return Ok(tags);
    };

    fn present(value: Option<String>) -> Option<String> {
        value.filter(|text| !text.trim().is_empty())
    }

    tags.title = present(tag.title().map(|value| value.to_string()));
    tags.artist = present(tag.artist().map(|value| value.to_string()));
    tags.album = present(tag.album().map(|value| value.to_string()));
    tags.genre = present(tag.genre().map(|value| value.to_string()));
    tags.comment = present(tag.comment().map(|value| value.to_string()));
    tags.album_artist = present(
        tag.get_string(&ItemKey::AlbumArtist)
            .map(|value| value.to_string()),
    );
    tags.isrc = present(
        tag.get_string(&ItemKey::Isrc)
            .map(|value| value.to_string()),
    );
    tags.year = tag.year();
    tags.track_number = tag.track();
    tags.disc_number = tag.disk();
    // Tempo lives in TBPM as an integer and in a free-text BPM item as a decimal.
    tags.bpm = [ItemKey::IntegerBpm, ItemKey::Bpm]
        .iter()
        .find_map(|key| tag.get_string(key))
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|bpm| bpm.is_finite() && *bpm > 0.0);
    tags.musical_key = present(
        tag.get_string(&ItemKey::InitialKey)
            .map(|value| value.to_string()),
    );

    Ok(tags)
}

/// Error type for metadata operations.
#[derive(Debug, Clone)]
pub enum MetadataError {
    /// Failed to read the file
    FileReadError(String),
    /// Failed to write to the file
    FileWriteError(String),
    /// Unsupported file format
    UnsupportedFormat,
}

impl std::fmt::Display for MetadataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetadataError::FileReadError(e) => write!(f, "Failed to read file: {}", e),
            MetadataError::FileWriteError(e) => write!(f, "Failed to write file: {}", e),
            MetadataError::UnsupportedFormat => write!(f, "Unsupported file format"),
        }
    }
}

impl std::error::Error for MetadataError {}

impl Track {
    /// Save metadata changes to the audio file.
    ///
    /// This writes the current metadata back to the file using lofty.
    /// Supported formats: MP3, FLAC, OGG, WAV (with ID3), M4A/AAC.
    pub fn save_metadata(&self) -> Result<(), MetadataError> {
        use lofty::config::WriteOptions;
        use lofty::tag::{Accessor, Tag, TagExt, TagType};

        // Open the file
        let tagged_file = Probe::open(&self.path)
            .map_err(|e| MetadataError::FileReadError(e.to_string()))?
            .read()
            .map_err(|e| MetadataError::FileReadError(e.to_string()))?;

        // Determine the best tag type for this file
        let tag_type = tagged_file
            .primary_tag()
            .map(|t| t.tag_type())
            .or_else(|| tagged_file.first_tag().map(|t| t.tag_type()))
            .unwrap_or_else(|| {
                // Default tag type based on file extension
                match self.path.extension().and_then(|e| e.to_str()) {
                    Some("mp3") => TagType::Id3v2,
                    Some("flac") => TagType::VorbisComments,
                    Some("ogg") => TagType::VorbisComments,
                    Some("m4a") | Some("aac") => TagType::Mp4Ilst,
                    Some("wav") => TagType::Id3v2,
                    _ => TagType::Id3v2,
                }
            });

        // Create or clone the tag
        let mut tag = tagged_file
            .primary_tag()
            .cloned()
            .or_else(|| tagged_file.first_tag().cloned())
            .unwrap_or_else(|| Tag::new(tag_type));

        // Update tag fields
        tag.set_title(self.title.clone());

        if let Some(ref artist) = self.artist {
            tag.set_artist(artist.clone());
        } else {
            tag.remove_artist();
        }

        if let Some(ref album) = self.album {
            tag.set_album(album.clone());
        } else {
            tag.remove_album();
        }

        if let Some(ref genre) = self.genre {
            tag.set_genre(genre.clone());
        } else {
            tag.remove_genre();
        }

        if let Some(year) = self.year {
            tag.set_year(year);
        } else {
            tag.remove_year();
        }

        if let Some(track_num) = self.track_number {
            tag.set_track(track_num);
        } else {
            tag.remove_track();
        }

        if let Some(ref comment) = self.comment {
            tag.set_comment(comment.clone());
        } else {
            tag.remove_comment();
        }

        // Save the tag to file
        tag.save_to_path(&self.path, WriteOptions::default())
            .map_err(|e| MetadataError::FileWriteError(e.to_string()))?;

        Ok(())
    }

    /// Update metadata fields from optional values.
    /// Only updates fields that are Some.
    pub fn update_metadata(
        &mut self,
        title: Option<String>,
        artist: Option<Option<String>>,
        album: Option<Option<String>>,
        genre: Option<Option<String>>,
        bpm: Option<Option<f32>>,
        key: Option<Option<String>>,
        year: Option<Option<u32>>,
        track_number: Option<Option<u32>>,
        comment: Option<Option<String>>,
        rating: Option<Option<u8>>,
    ) {
        if let Some(t) = title {
            self.title = t;
        }
        if let Some(a) = artist {
            self.artist = a;
        }
        if let Some(a) = album {
            self.album = a;
        }
        if let Some(g) = genre {
            self.genre = g;
        }
        if let Some(b) = bpm {
            self.bpm = b;
        }
        if let Some(k) = key {
            self.key = k;
        }
        if let Some(y) = year {
            self.year = y;
        }
        if let Some(t) = track_number {
            self.track_number = t;
        }
        if let Some(c) = comment {
            self.comment = c;
        }
        if let Some(r) = rating {
            self.rating = r;
        }
    }
}
