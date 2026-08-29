//! Multi-channel audio engine using rodio.
//!
//! Provides configurable deck-based playback for DJ applications.

use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// Errors that can occur during audio operations.
#[derive(Debug, Error)]
pub enum AudioError {
    #[error("Failed to open audio file: {0}")]
    FileOpen(#[from] std::io::Error),
    #[error("Failed to decode audio: {0}")]
    Decode(#[from] rodio::decoder::DecoderError),
    #[error("Failed to initialize audio output stream")]
    StreamInit,
    #[error("Invalid deck index: {0}")]
    InvalidDeck(usize),
    #[error("No track loaded on deck {0}")]
    NoTrackLoaded(usize),
}

/// State of a single deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckState {
    /// No track loaded
    Empty,
    /// Track loaded but not playing
    Stopped,
    /// Track is playing
    Playing,
    /// Track is paused
    Paused,
}

/// A single playback deck with its own sink and controls.
pub struct Deck {
    sink: Sink,
    stream_handle: Arc<OutputStreamHandle>,
    state: DeckState,
    /// The file currently decoded, kept so `stop` can rewind by re-decoding it.
    source_path: Option<PathBuf>,
    volume: f32,
    /// Track if audio has been loaded (separate from track metadata)
    has_audio: bool,
}

impl Deck {
    /// Create a new deck with the given output stream handle.
    fn new(stream_handle: Arc<OutputStreamHandle>) -> Result<Self, AudioError> {
        let sink = Sink::try_new(&stream_handle).map_err(|_| AudioError::StreamInit)?;
        sink.pause(); // Start paused

        Ok(Self {
            sink,
            stream_handle,
            state: DeckState::Empty,
            source_path: None,
            volume: 1.0,
            has_audio: false,
        })
    }

    /// Decode `path` onto this deck.
    ///
    /// A deck plays a file. What that file means — which canonical track it is, and
    /// whether it is an owned original or a cache copy — belongs to the catalog, not
    /// to audio output.
    pub fn load(&mut self, path: &Path) -> Result<(), AudioError> {
        // Create a fresh sink to ensure clean state
        self.sink = Sink::try_new(&self.stream_handle).map_err(|_| AudioError::StreamInit)?;

        // Open and decode the file
        let file = File::open(path)?;
        let source = Decoder::new(BufReader::new(file))?;

        // Append source and configure
        self.sink.append(source);
        self.sink.set_volume(self.volume);
        self.sink.pause();

        self.source_path = Some(path.to_path_buf());
        self.state = DeckState::Stopped;
        self.has_audio = true;

        Ok(())
    }

    /// Start or resume playback.
    pub fn play(&mut self) {
        if self.has_audio {
            self.sink.play();
            self.state = DeckState::Playing;
        }
    }

    /// Pause playback.
    pub fn pause(&mut self) {
        self.sink.pause();
        if self.has_audio {
            self.state = DeckState::Paused;
        }
    }

    /// Stop playback and reset position.
    pub fn stop(&mut self) {
        // Create a fresh sink to reset position
        if let Ok(new_sink) = Sink::try_new(&self.stream_handle) {
            self.sink = new_sink;
            self.sink.set_volume(self.volume);
            self.sink.pause();
        }

        // Reload the file if we have one
        if let Some(ref path) = self.source_path {
            if let Ok(file) = File::open(path) {
                if let Ok(source) = Decoder::new(BufReader::new(file)) {
                    self.sink.append(source);
                    self.sink.pause();
                    self.has_audio = true;
                }
            }
        }

        self.state = DeckState::Stopped;
    }

    /// Set volume (0.0 to 1.0+).
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.max(0.0);
        self.sink.set_volume(self.volume);
    }

    /// Get current volume.
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Get current state.
    pub fn state(&self) -> DeckState {
        self.state
    }

    /// Check if the track has finished playing.
    /// This is different from is_empty - it only returns true if
    /// we had audio and it finished.
    pub fn is_finished(&self) -> bool {
        self.has_audio && self.sink.empty()
    }

    /// Check if the deck has audio loaded.
    pub fn has_audio(&self) -> bool {
        self.source_path.is_some()
    }

    /// Get playback position (if supported).
    pub fn position(&self) -> Duration {
        self.sink.get_pos()
    }

    /// Seek to a position.
    pub fn seek(&mut self, position: Duration) -> Result<(), AudioError> {
        self.sink
            .try_seek(position)
            .map_err(|_| AudioError::StreamInit)
    }

    /// Check if currently playing (based on sink state, not our state tracking).
    pub fn is_playing(&self) -> bool {
        !self.sink.is_paused() && !self.sink.empty()
    }
}

/// Multi-deck audio engine for DJ playback.
pub struct AudioEngine {
    // Keep the stream alive - it must outlive all sinks
    _stream: OutputStream,
    stream_handle: Arc<OutputStreamHandle>,
    decks: Vec<Deck>,
}

impl AudioEngine {
    /// Create a new audio engine with the specified number of decks.
    pub fn new(deck_count: usize) -> Result<Self, AudioError> {
        let (stream, stream_handle) =
            OutputStream::try_default().map_err(|_| AudioError::StreamInit)?;
        let stream_handle = Arc::new(stream_handle);

        let mut decks = Vec::with_capacity(deck_count);
        for _ in 0..deck_count {
            decks.push(Deck::new(stream_handle.clone())?);
        }

        Ok(Self {
            _stream: stream,
            stream_handle,
            decks,
        })
    }

    /// Create a standard 2-deck setup.
    pub fn two_deck() -> Result<Self, AudioError> {
        Self::new(2)
    }

    /// Create a 4-deck setup.
    pub fn four_deck() -> Result<Self, AudioError> {
        Self::new(4)
    }

    /// Get the number of decks.
    pub fn deck_count(&self) -> usize {
        self.decks.len()
    }

    /// Add a new deck to the engine.
    pub fn add_deck(&mut self) -> Result<usize, AudioError> {
        let deck = Deck::new(self.stream_handle.clone())?;
        self.decks.push(deck);
        Ok(self.decks.len() - 1)
    }

    /// Get a reference to a deck by index.
    pub fn deck(&self, index: usize) -> Result<&Deck, AudioError> {
        self.decks.get(index).ok_or(AudioError::InvalidDeck(index))
    }

    /// Get a mutable reference to a deck by index.
    pub fn deck_mut(&mut self, index: usize) -> Result<&mut Deck, AudioError> {
        self.decks
            .get_mut(index)
            .ok_or(AudioError::InvalidDeck(index))
    }

    /// Decode a file onto a specific deck.
    pub fn load_file(&mut self, deck_index: usize, path: &Path) -> Result<(), AudioError> {
        self.deck_mut(deck_index)?.load(path)
    }

    /// Play a specific deck.
    pub fn play(&mut self, deck_index: usize) -> Result<(), AudioError> {
        self.deck_mut(deck_index)?.play();
        Ok(())
    }

    /// Pause a specific deck.
    pub fn pause(&mut self, deck_index: usize) -> Result<(), AudioError> {
        self.deck_mut(deck_index)?.pause();
        Ok(())
    }

    /// Stop a specific deck.
    pub fn stop(&mut self, deck_index: usize) -> Result<(), AudioError> {
        self.deck_mut(deck_index)?.stop();
        Ok(())
    }

    /// Set volume for a specific deck.
    pub fn set_volume(&mut self, deck_index: usize, volume: f32) -> Result<(), AudioError> {
        self.deck_mut(deck_index)?.set_volume(volume);
        Ok(())
    }

    /// Stop all decks.
    pub fn stop_all(&mut self) {
        for deck in &mut self.decks {
            deck.stop();
        }
    }

    /// Pause all decks.
    pub fn pause_all(&mut self) {
        for deck in &mut self.decks {
            deck.pause();
        }
    }
}
