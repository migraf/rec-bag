//! Multi-channel audio engine using rodio.
//!
//! Provides configurable deck-based playback for DJ applications.

use crate::mixer::{crossfader_gains, ChannelControls, ChannelMix, MixedSource};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};
use std::fs::File;
use std::io::{BufReader, Cursor};
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

/// Audio a deck can decode: a file on disk, or a stream already held in memory.
///
/// Memory input exists for streamed provider audio, which must not be written to disk. Cloning
/// shares the buffer rather than copying it.
#[derive(Clone, PartialEq, Eq)]
pub enum AudioInput {
    File(PathBuf),
    Memory(Arc<[u8]>),
}

impl AudioInput {
    /// The file behind this input, when it is one.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::Memory(_) => None,
        }
    }

    fn decode(&self) -> Result<Box<dyn rodio::Source<Item = i16> + Send>, AudioError> {
        Ok(match self {
            Self::File(path) => Box::new(Decoder::new(BufReader::new(File::open(path)?))?),
            Self::Memory(bytes) => Box::new(Decoder::new(Cursor::new(bytes.clone()))?),
        })
    }
}

impl std::fmt::Debug for AudioInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => formatter.debug_tuple("File").field(path).finish(),
            Self::Memory(bytes) => write!(formatter, "Memory({} bytes)", bytes.len()),
        }
    }
}

impl From<PathBuf> for AudioInput {
    fn from(path: PathBuf) -> Self {
        Self::File(path)
    }
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
    /// The audio currently decoded, kept so `stop` can rewind by re-decoding it.
    source: Option<AudioInput>,
    volume: f32,
    /// Where the channel strip's knobs sit.
    mix: ChannelMix,
    /// How loud the crossfader lets this deck play.
    crossfader_gain: f32,
    /// The channel strip as the audio thread reads it, shared by every track loaded here.
    controls: Arc<ChannelControls>,
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
            source: None,
            volume: 1.0,
            mix: ChannelMix::neutral(),
            crossfader_gain: 1.0,
            controls: Arc::new(ChannelControls::default()),
            has_audio: false,
        })
    }

    /// Decode `input` through this deck's channel strip.
    fn decode(&self, input: &AudioInput) -> Result<impl Source<Item = f32> + Send, AudioError> {
        Ok(MixedSource::new(
            input.decode()?.convert_samples(),
            self.controls.clone(),
        ))
    }

    /// Decode `input` onto this deck.
    ///
    /// A deck plays audio. What that audio means — which canonical track it is, and
    /// whether it is an owned original, a cache copy, or a provider stream — belongs to
    /// the catalog, not to audio output.
    pub fn load(&mut self, input: &AudioInput) -> Result<(), AudioError> {
        // Decode first, so a failure leaves the current track playing.
        let source = self.decode(input)?;
        // Create a fresh sink to ensure clean state
        self.sink = Sink::try_new(&self.stream_handle).map_err(|_| AudioError::StreamInit)?;

        // Append source and configure
        self.sink.append(source);
        self.sink.set_volume(self.volume);
        self.sink.pause();

        self.source = Some(input.clone());
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

        // Reload the audio if we have some
        if let Some(input) = &self.source {
            if let Ok(source) = self.decode(input) {
                self.sink.append(source);
                self.sink.pause();
                self.has_audio = true;
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

    /// Turn the channel strip's knobs.
    pub fn set_mix(&mut self, mix: ChannelMix) {
        self.mix = mix;
        self.controls.set(self.mix, self.crossfader_gain);
    }

    /// Where the channel strip's knobs sit.
    pub fn mix(&self) -> ChannelMix {
        self.mix
    }

    fn set_crossfader_gain(&mut self, gain: f32) {
        self.crossfader_gain = gain;
        self.controls.set(self.mix, self.crossfader_gain);
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
        self.source.is_some()
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
    crossfader: f32,
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
            crossfader: 0.0,
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
        self.load(deck_index, &AudioInput::File(path.to_path_buf()))
    }

    /// Decode a file or an in-memory stream onto a specific deck.
    pub fn load(&mut self, deck_index: usize, input: &AudioInput) -> Result<(), AudioError> {
        self.deck_mut(deck_index)?.load(input)
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

    /// Move the crossfader, from -1 (only deck one) to 1 (only deck two).
    ///
    /// Deck one sits on the left of the crossfader and deck two on the right; any
    /// further deck bypasses it.
    pub fn set_crossfader(&mut self, position: f32) {
        self.crossfader = position.clamp(-1.0, 1.0);
        let (left, right) = crossfader_gains(self.crossfader);
        for (index, deck) in self.decks.iter_mut().enumerate().take(2) {
            deck.set_crossfader_gain(if index == 0 { left } else { right });
        }
    }

    /// Where the crossfader sits.
    pub fn crossfader(&self) -> f32 {
        self.crossfader
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
