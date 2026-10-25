//! One playback channel that a Loaded Track occupies.
//!
//! Views ask a deck to play, pause, seek, or load. They never lock the audio engine,
//! never name a deck by a literal index, and never reconcile display state by hand:
//! the deck owns the lock, the index, and the reconciliation, so playback behaves the
//! same wherever it is driven from.

use crate::services::catalog::LoadedTrack;
use dioxus::prelude::*;
use rec_core::{AudioEngine, AudioInput, ChannelMix, DeckState, Track};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use ui::{DeckDisplayState, StreamCredit};
use uuid::Uuid;

/// How many decks the application runs.
pub const DECK_COUNT: usize = 2;

/// What a deck needs from audio output, independent of any audio device.
///
/// Two adapters satisfy it: [`AudioEngine`] in the running application and an
/// in-memory fake in this module's tests. Every method takes the deck's own index, so
/// an out-of-range index is this trait's problem and never a caller's.
pub trait DeckAudio {
    fn load(&mut self, index: usize, input: &AudioInput) -> bool;
    fn play(&mut self, index: usize);
    fn pause(&mut self, index: usize);
    fn stop(&mut self, index: usize);
    fn seek(&mut self, index: usize, position: Duration);
    fn set_volume(&mut self, index: usize, volume: f32);
    fn set_mix(&mut self, index: usize, mix: ChannelMix);
    fn state(&self, index: usize) -> DeckState;
    fn position(&self, index: usize) -> Duration;
    fn volume(&self, index: usize) -> f32;
    fn mix(&self, index: usize) -> ChannelMix;
    fn is_finished(&self, index: usize) -> bool;
    /// Move the crossfader, from -1 (only deck one) to 1 (only deck two).
    fn set_crossfader(&mut self, position: f32);
    fn crossfader(&self) -> f32;
}

impl DeckAudio for AudioEngine {
    fn load(&mut self, index: usize, input: &AudioInput) -> bool {
        AudioEngine::load(self, index, input).is_ok()
    }

    fn play(&mut self, index: usize) {
        let _ = AudioEngine::play(self, index);
    }

    fn pause(&mut self, index: usize) {
        let _ = AudioEngine::pause(self, index);
    }

    fn stop(&mut self, index: usize) {
        let _ = AudioEngine::stop(self, index);
    }

    fn seek(&mut self, index: usize, position: Duration) {
        if let Ok(deck) = self.deck_mut(index) {
            let _ = deck.seek(position);
        }
    }

    fn set_volume(&mut self, index: usize, volume: f32) {
        let _ = AudioEngine::set_volume(self, index, volume);
    }

    fn set_mix(&mut self, index: usize, mix: ChannelMix) {
        if let Ok(deck) = self.deck_mut(index) {
            deck.set_mix(mix);
        }
    }

    fn state(&self, index: usize) -> DeckState {
        self.deck(index)
            .map(|deck| deck.state())
            .unwrap_or(DeckState::Empty)
    }

    fn position(&self, index: usize) -> Duration {
        self.deck(index)
            .map(|deck| deck.position())
            .unwrap_or(Duration::ZERO)
    }

    fn volume(&self, index: usize) -> f32 {
        self.deck(index).map(|deck| deck.volume()).unwrap_or(1.0)
    }

    fn mix(&self, index: usize) -> ChannelMix {
        self.deck(index).map(|deck| deck.mix()).unwrap_or_default()
    }

    fn is_finished(&self, index: usize) -> bool {
        self.deck(index)
            .map(|deck| deck.is_finished())
            .unwrap_or(false)
    }

    fn set_crossfader(&mut self, position: f32) {
        AudioEngine::set_crossfader(self, position);
    }

    fn crossfader(&self) -> f32 {
        AudioEngine::crossfader(self)
    }
}

/// Audio output shared by every deck.
pub type SharedAudio = Arc<StdMutex<dyn DeckAudio>>;

/// Everything a view renders about a deck other than its Loaded Track.
#[derive(Debug, Clone, PartialEq)]
pub struct DeckSnapshot {
    pub state: DeckDisplayState,
    pub position: Duration,
    pub volume: f32,
    pub mix: ChannelMix,
}

impl Default for DeckSnapshot {
    fn default() -> Self {
        Self {
            state: DeckDisplayState::Empty,
            position: Duration::ZERO,
            volume: 1.0,
            mix: ChannelMix::neutral(),
        }
    }
}

/// One playback channel that a Loaded Track occupies.
#[derive(Clone, Copy)]
pub struct Deck {
    audio: Signal<SharedAudio>,
    index: usize,
    loaded: Signal<Option<LoadedTrack>>,
    /// The track being prepared for this deck, which can take seconds when it streams.
    loading: Signal<Option<Pending>>,
    live: Signal<DeckSnapshot>,
}

/// A track a deck has been asked to load and is still preparing.
#[derive(Clone, PartialEq)]
struct Pending {
    track_id: Uuid,
    /// What to call it while it loads; the projection's record when it holds one.
    title: String,
}

impl Deck {
    /// Open deck `index` on the given audio output. Must run inside a Dioxus scope.
    pub fn new(audio: Signal<SharedAudio>, index: usize) -> Self {
        Self {
            audio,
            index,
            loaded: Signal::new(None),
            loading: Signal::new(None),
            live: Signal::new(DeckSnapshot::default()),
        }
    }

    /// The deck's user-facing number, counting from one.
    pub fn number(&self) -> usize {
        self.index + 1
    }

    /// The Loaded Track's catalog record, if this deck holds one.
    pub fn track(&self) -> Option<Track> {
        self.loaded
            .read()
            .as_ref()
            .map(|loaded| loaded.track.clone())
    }

    /// Who is streaming the Loaded Track, when it is not a local file.
    pub fn stream(&self) -> Option<StreamCredit> {
        let loaded = self.loaded.read();
        let origin = loaded.as_ref()?.streamed_from.as_ref()?;
        Some(StreamCredit {
            source_type: origin.source_type,
            url: origin.permalink.clone(),
            preview: origin.preview,
        })
    }

    /// The in-memory stream of `track_id`, when this deck holds that track as a stream.
    pub fn streamed_audio(&self, track_id: Uuid) -> Option<AudioInput> {
        let loaded = self.loaded.read();
        let loaded = loaded.as_ref()?;
        (loaded.id() == track_id && loaded.streamed_from.is_some()).then(|| loaded.audio.clone())
    }

    /// Whether this deck holds the given canonical track.
    pub fn holds(&self, track_id: Uuid) -> bool {
        self.loaded
            .read()
            .as_ref()
            .is_some_and(|loaded| loaded.id() == track_id)
    }

    pub fn state(&self) -> DeckDisplayState {
        self.live.read().state.clone()
    }

    pub fn position(&self) -> Duration {
        self.live.read().position
    }

    pub fn volume(&self) -> f32 {
        self.live.read().volume
    }

    /// Where this deck's channel strip knobs sit.
    pub fn mix(&self) -> ChannelMix {
        self.live.read().mix
    }

    pub fn is_playing(&self) -> bool {
        self.state() == DeckDisplayState::Playing
    }

    /// The title of the track this deck is preparing, while it prepares one.
    pub fn loading(&self) -> Option<String> {
        self.loading
            .read()
            .as_ref()
            .map(|pending| pending.title.clone())
    }

    /// Start preparing `track_id` for this deck.
    ///
    /// Returns false when the deck is already preparing that same track, so a repeated
    /// gesture — a double click while a stream downloads — starts nothing a second time.
    /// Asking for a different track takes the deck over: the earlier one is discarded when
    /// it arrives.
    pub fn begin_loading(&self, track_id: Uuid, title: String) -> bool {
        if self.is_loading(track_id) {
            return false;
        }
        let mut loading = self.loading;
        loading.set(Some(Pending { track_id, title }));
        true
    }

    /// Put a prepared track on this deck, unless a later request took the deck over while
    /// it was being prepared.
    pub fn finish_loading(&self, loaded: LoadedTrack) -> bool {
        if !self.is_loading(loaded.id()) {
            return false;
        }
        self.clear_loading();
        self.load(loaded)
    }

    /// Give up preparing `track_id`, leaving whatever the deck holds.
    pub fn abandon_loading(&self, track_id: Uuid) {
        if self.is_loading(track_id) {
            self.clear_loading();
        }
    }

    fn is_loading(&self, track_id: Uuid) -> bool {
        self.loading
            .read()
            .as_ref()
            .is_some_and(|pending| pending.track_id == track_id)
    }

    fn clear_loading(&self) {
        let mut loading = self.loading;
        loading.set(None);
    }

    /// Put a Loaded Track on this deck.
    ///
    /// The audio engine decodes the loaded track's resolved audio; the deck keeps the
    /// track's own identity, so a cached copy of a remote asset never becomes the track.
    pub fn load(&self, loaded: LoadedTrack) -> bool {
        if !self.with_audio(|audio, index| audio.load(index, &loaded.audio)) {
            return false;
        }
        self.set_loaded(Some(loaded));
        self.refresh();
        true
    }

    pub fn play(&self) {
        self.with_audio(|audio, index| audio.play(index));
        self.refresh();
    }

    pub fn pause(&self) {
        self.with_audio(|audio, index| audio.pause(index));
        self.refresh();
    }

    pub fn stop(&self) {
        self.with_audio(|audio, index| audio.stop(index));
        self.refresh();
    }

    /// Play if paused or stopped, pause if playing.
    pub fn toggle(&self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    pub fn seek(&self, position: Duration) {
        self.with_audio(|audio, index| audio.seek(index, position));
        self.refresh();
        // Show the new position immediately rather than waiting for the next refresh.
        let mut live = self.live;
        live.write().position = position;
    }

    pub fn set_volume(&self, volume: f32) {
        self.with_audio(|audio, index| audio.set_volume(index, volume));
        self.refresh();
    }

    /// Turn this deck's trim and EQ knobs.
    pub fn set_mix(&self, mix: ChannelMix) {
        self.with_audio(|audio, index| audio.set_mix(index, mix));
        self.refresh();
    }

    /// Apply `edit` to the Loaded Track when this deck holds `track_id`.
    ///
    /// Analysis results and metadata edits reach a playing track through here, so
    /// every deck stays current without any caller enumerating the decks by hand.
    pub fn edit_track(&self, track_id: Uuid, edit: impl FnOnce(&mut Track)) -> bool {
        if !self.holds(track_id) {
            return false;
        }
        let mut loaded = match self.loaded.read().clone() {
            Some(loaded) => loaded,
            None => return false,
        };
        edit(&mut loaded.track);
        self.set_loaded(Some(loaded));
        true
    }

    /// Re-read the deck's live state from the audio output.
    ///
    /// Skips the update when the audio output is busy, so a refresh never blocks the
    /// interface. Writing only on change keeps an idle deck from re-rendering.
    pub fn refresh(&self) {
        let observed = {
            let shared = self.audio.read().clone();
            let Ok(audio) = shared.try_lock() else {
                return;
            };
            observe(&*audio, self.index)
        };
        if *self.live.read() != observed {
            let mut live = self.live;
            live.set(observed);
        }
    }

    fn set_loaded(&self, loaded: Option<LoadedTrack>) {
        let mut slot = self.loaded;
        slot.set(loaded);
    }

    fn with_audio<T>(&self, act: impl FnOnce(&mut dyn DeckAudio, usize) -> T) -> T
    where
        T: Default,
    {
        let shared = self.audio.read().clone();
        let Ok(mut audio) = shared.lock() else {
            return T::default();
        };
        act(&mut *audio, self.index)
    }
}

/// Derive what a view shows from what the audio output reports.
///
/// A finished sink still reports `Playing`, so finished-track detection lives here
/// rather than in each view's polling loop.
fn observe(audio: &dyn DeckAudio, index: usize) -> DeckSnapshot {
    let state = match audio.state(index) {
        DeckState::Empty => DeckDisplayState::Empty,
        DeckState::Stopped => DeckDisplayState::Stopped,
        DeckState::Paused => DeckDisplayState::Paused,
        DeckState::Playing if audio.is_finished(index) => DeckDisplayState::Stopped,
        DeckState::Playing => DeckDisplayState::Playing,
    };
    DeckSnapshot {
        state,
        position: audio.position(index),
        volume: audio.volume(index),
        mix: audio.mix(index),
    }
}

/// The crossfader between deck one and deck two.
#[derive(Clone, Copy)]
pub struct Crossfader {
    audio: Signal<SharedAudio>,
    position: Signal<f32>,
}

impl Crossfader {
    /// Open the crossfader on the given audio output. Must run inside a Dioxus scope.
    pub fn new(audio: Signal<SharedAudio>) -> Self {
        let position = audio
            .read()
            .lock()
            .map(|audio| audio.crossfader())
            .unwrap_or(0.0);
        Self {
            audio,
            position: Signal::new(position),
        }
    }

    /// Where the crossfader sits, from -1 (only deck one) to 1 (only deck two).
    pub fn position(&self) -> f32 {
        *self.position.read()
    }

    pub fn set(&self, position: f32) {
        let shared = self.audio.read().clone();
        let Ok(mut audio) = shared.lock() else {
            return;
        };
        audio.set_crossfader(position);
        let mut slot = self.position;
        slot.set(audio.crossfader());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support::with_runtime;
    use std::path::PathBuf;

    /// An audio output that records what it was asked to do, so deck behaviour can be
    /// tested without an audio device.
    #[derive(Default)]
    pub(super) struct FakeAudio {
        pub(super) decks: Vec<FakeDeck>,
        crossfader: f32,
        calls: Vec<String>,
    }

    #[derive(Clone)]
    pub(super) struct FakeDeck {
        state: DeckState,
        pub(super) position: Duration,
        volume: f32,
        mix: ChannelMix,
        finished: bool,
        refuse_load: bool,
        loaded_from: Option<AudioInput>,
    }

    impl Default for FakeDeck {
        fn default() -> Self {
            Self {
                state: DeckState::Empty,
                position: Duration::ZERO,
                volume: 1.0,
                mix: ChannelMix::neutral(),
                finished: false,
                refuse_load: false,
                loaded_from: None,
            }
        }
    }

    impl FakeAudio {
        pub(super) fn new() -> Self {
            Self {
                decks: vec![FakeDeck::default(); DECK_COUNT],
                crossfader: 0.0,
                calls: Vec::new(),
            }
        }
    }

    impl DeckAudio for FakeAudio {
        fn load(&mut self, index: usize, input: &AudioInput) -> bool {
            self.calls.push(format!("load {index}"));
            if self.decks[index].refuse_load {
                return false;
            }
            self.decks[index].loaded_from = Some(input.clone());
            self.decks[index].state = DeckState::Stopped;
            true
        }
        fn play(&mut self, index: usize) {
            self.calls.push(format!("play {index}"));
            self.decks[index].state = DeckState::Playing;
        }
        fn pause(&mut self, index: usize) {
            self.calls.push(format!("pause {index}"));
            self.decks[index].state = DeckState::Paused;
        }
        fn stop(&mut self, index: usize) {
            self.calls.push(format!("stop {index}"));
            self.decks[index].state = DeckState::Stopped;
            self.decks[index].position = Duration::ZERO;
        }
        fn seek(&mut self, index: usize, position: Duration) {
            self.calls.push(format!("seek {index}"));
            self.decks[index].position = position;
        }
        fn set_volume(&mut self, index: usize, volume: f32) {
            self.decks[index].volume = volume;
        }
        fn set_mix(&mut self, index: usize, mix: ChannelMix) {
            self.decks[index].mix = mix;
        }
        fn state(&self, index: usize) -> DeckState {
            self.decks[index].state
        }
        fn position(&self, index: usize) -> Duration {
            self.decks[index].position
        }
        fn volume(&self, index: usize) -> f32 {
            self.decks[index].volume
        }
        fn mix(&self, index: usize) -> ChannelMix {
            self.decks[index].mix
        }
        fn is_finished(&self, index: usize) -> bool {
            self.decks[index].finished
        }
        fn set_crossfader(&mut self, position: f32) {
            self.crossfader = position.clamp(-1.0, 1.0);
        }
        fn crossfader(&self) -> f32 {
            self.crossfader
        }
    }

    /// Decks need Dioxus signals, which need a scope to be owned by.
    fn with_decks(test: impl FnOnce([Deck; DECK_COUNT], Arc<StdMutex<FakeAudio>>)) {
        with_runtime(|| {
            let fake = Arc::new(StdMutex::new(FakeAudio::new()));
            let shared: SharedAudio = fake.clone();
            let audio = Signal::new_in_scope(shared, ScopeId::ROOT);
            let decks = [deck_in_scope(audio, 0), deck_in_scope(audio, 1)];
            test(decks, fake);
        })
    }

    fn deck_in_scope(audio: Signal<SharedAudio>, index: usize) -> Deck {
        Deck {
            audio,
            index,
            loaded: Signal::new_in_scope(None, ScopeId::ROOT),
            loading: Signal::new_in_scope(None, ScopeId::ROOT),
            live: Signal::new_in_scope(DeckSnapshot::default(), ScopeId::ROOT),
        }
    }

    /// A canonical track that has been prepared for a deck: its own record, plus the
    /// disposable cache file the audio engine will decode.
    fn a_loaded_track(cache: &str) -> LoadedTrack {
        let mut track = Track::new_without_metadata(PathBuf::new());
        track.title = "Canonical".into();
        LoadedTrack {
            track,
            audio: AudioInput::File(PathBuf::from(cache)),
            cached: true,
            streamed_from: None,
        }
    }

    #[test]
    fn a_deck_drives_its_own_index() {
        with_decks(|decks, fake| {
            decks[1].play();
            decks[0].pause();
            assert_eq!(fake.lock().unwrap().calls, vec!["play 1", "pause 0"]);
        });
    }

    #[test]
    fn deck_numbers_count_from_one() {
        with_decks(|decks, _| {
            assert_eq!(decks[0].number(), 1);
            assert_eq!(decks[1].number(), 2);
        });
    }

    #[test]
    fn loading_plays_the_resolved_path_but_keeps_the_track() {
        with_decks(|decks, fake| {
            let loaded = a_loaded_track("/cache/drive/abc.mp3");
            let id = loaded.id();
            assert!(decks[0].load(loaded));
            assert_eq!(
                fake.lock().unwrap().decks[0].loaded_from,
                Some(AudioInput::File(PathBuf::from("/cache/drive/abc.mp3")))
            );
            // The cache copy is what plays; it never becomes the track's identity.
            assert_eq!(
                decks[0].track().map(|track| track.path),
                Some(PathBuf::new())
            );
            assert!(decks[0].holds(id));
        });
    }

    #[test]
    fn asking_again_for_the_track_a_deck_is_preparing_starts_nothing() {
        with_decks(|decks, fake| {
            let loaded = a_loaded_track("/cache/stream.mp3");
            let id = loaded.id();
            assert!(decks[0].begin_loading(id, "Canonical".into()));
            assert!(!decks[0].begin_loading(id, "Canonical".into()));
            // The other deck is a separate, deliberate request.
            assert!(decks[1].begin_loading(id, "Canonical".into()));
            assert_eq!(decks[0].loading().as_deref(), Some("Canonical"));

            assert!(decks[0].finish_loading(loaded));

            assert_eq!(decks[0].loading(), None);
            assert!(decks[0].holds(id));
            assert_eq!(fake.lock().unwrap().calls, vec!["load 0"]);
        });
    }

    #[test]
    fn a_track_superseded_while_preparing_is_discarded_when_it_arrives() {
        with_decks(|decks, fake| {
            let first = a_loaded_track("/cache/first.mp3");
            let second = a_loaded_track("/cache/second.mp3");
            assert!(decks[0].begin_loading(first.id(), "First".into()));
            assert!(decks[0].begin_loading(second.id(), "Second".into()));

            assert!(!decks[0].finish_loading(first.clone()));
            assert_eq!(decks[0].loading().as_deref(), Some("Second"));
            // A late failure of the superseded track leaves the newer request alone.
            decks[0].abandon_loading(first.id());
            assert_eq!(decks[0].loading().as_deref(), Some("Second"));

            assert!(decks[0].finish_loading(second.clone()));
            assert!(decks[0].holds(second.id()));
            assert_eq!(fake.lock().unwrap().calls, vec!["load 0"]);
        });
    }

    #[test]
    fn a_failed_load_clears_the_indicator_and_allows_a_retry() {
        with_decks(|decks, _| {
            let id = Uuid::new_v4();
            assert!(decks[0].begin_loading(id, "Flaky".into()));
            decks[0].abandon_loading(id);
            assert_eq!(decks[0].loading(), None);
            assert!(decks[0].begin_loading(id, "Flaky".into()));
        });
    }

    #[test]
    fn only_a_streamed_track_offers_its_audio_for_analysis() {
        with_decks(|decks, _| {
            let file = a_loaded_track("/cache/owned.mp3");
            let mut stream = a_loaded_track("/unused");
            stream.audio = AudioInput::Memory(Arc::from(&b"mp3"[..]));
            stream.streamed_from = Some(crate::services::catalog::StreamOrigin {
                source_type: rec_core::SourceType::SoundCloud,
                permalink: None,
                preview: false,
            });
            let (file_id, stream_id) = (file.id(), stream.id());
            decks[0].load(file);
            decks[1].load(stream);

            assert_eq!(decks[0].streamed_audio(file_id), None);
            assert_eq!(decks[1].streamed_audio(file_id), None);
            assert_eq!(
                decks[1].streamed_audio(stream_id),
                Some(AudioInput::Memory(Arc::from(&b"mp3"[..])))
            );
        });
    }

    #[test]
    fn a_deck_that_cannot_decode_does_not_claim_the_track() {
        with_decks(|decks, fake| {
            fake.lock().unwrap().decks[0].refuse_load = true;
            assert!(!decks[0].load(a_loaded_track("/cache/x.mp3")));
            assert!(decks[0].track().is_none());
        });
    }

    #[test]
    fn a_finished_track_reads_as_stopped_not_playing() {
        with_decks(|decks, fake| {
            decks[0].play();
            assert_eq!(decks[0].state(), DeckDisplayState::Playing);
            fake.lock().unwrap().decks[0].finished = true;
            decks[0].refresh();
            assert_eq!(decks[0].state(), DeckDisplayState::Stopped);
        });
    }

    #[test]
    fn toggle_follows_the_audio_output_not_the_last_click() {
        with_decks(|decks, fake| {
            decks[0].toggle();
            assert_eq!(decks[0].state(), DeckDisplayState::Playing);
            // Something outside the view paused the deck.
            fake.lock().unwrap().decks[0].state = DeckState::Paused;
            decks[0].refresh();
            decks[0].toggle();
            assert_eq!(decks[0].state(), DeckDisplayState::Playing);
        });
    }

    #[test]
    fn seeking_shows_the_new_position_immediately() {
        with_decks(|decks, _| {
            decks[0].seek(Duration::from_secs(42));
            assert_eq!(decks[0].position(), Duration::from_secs(42));
        });
    }

    #[test]
    fn only_the_deck_holding_the_track_is_edited() {
        with_decks(|decks, _| {
            let loaded = a_loaded_track("/cache/a.mp3");
            let id = loaded.id();
            decks[0].load(loaded);
            decks[1].load(a_loaded_track("/cache/b.mp3"));

            assert!(decks[0].edit_track(id, |track| track.set_bpm(128.0)));
            assert!(!decks[1].edit_track(id, |track| track.set_bpm(128.0)));

            assert_eq!(decks[0].track().and_then(|t| t.bpm), Some(128.0));
            assert_eq!(decks[1].track().and_then(|t| t.bpm), None);
        });
    }

    #[test]
    fn editing_an_empty_deck_changes_nothing() {
        with_decks(|decks, _| {
            assert!(!decks[0].edit_track(Uuid::new_v4(), |track| track.set_bpm(1.0)));
        });
    }

    #[test]
    fn a_volume_change_reaches_the_snapshot() {
        with_decks(|decks, _| {
            decks[1].set_volume(0.25);
            assert_eq!(decks[1].volume(), 0.25);
            assert_eq!(decks[0].volume(), 1.0);
        });
    }

    #[test]
    fn a_turned_knob_reaches_its_own_deck_only() {
        with_decks(|decks, fake| {
            let mix = ChannelMix {
                low: -1.0,
                ..ChannelMix::neutral()
            };
            decks[0].set_mix(mix);
            assert_eq!(decks[0].mix(), mix);
            assert_eq!(decks[1].mix(), ChannelMix::neutral());
            assert_eq!(fake.lock().unwrap().decks[0].mix, mix);
        });
    }

    #[test]
    fn the_crossfader_shows_where_the_audio_output_put_it() {
        with_runtime(|| {
            let fake = Arc::new(StdMutex::new(FakeAudio::new()));
            let shared: SharedAudio = fake.clone();
            let crossfader = Crossfader {
                audio: Signal::new_in_scope(shared, ScopeId::ROOT),
                position: Signal::new_in_scope(0.0, ScopeId::ROOT),
            };
            crossfader.set(-0.5);
            assert_eq!(crossfader.position(), -0.5);
            crossfader.set(3.0);
            assert_eq!(crossfader.position(), 1.0);
            assert_eq!(fake.lock().unwrap().crossfader, 1.0);
        });
    }
}

/// Does what a deck observes actually reach the interface?
///
/// The deck's own tests drive a `Deck` directly. These drive the whole seam the DJ view
/// uses — a real `VirtualDom`, the real `DeckControl`, and a deck refreshed the way the
/// refresh loop refreshes it — because a playhead that moves in a signal but not on
/// screen is exactly the bug this catches.
#[cfg(test)]
mod view_tests {
    use super::tests::FakeAudio;
    use super::*;
    use rec_core::Track;
    use std::cell::RefCell;
    use std::path::PathBuf;

    thread_local! {
        /// Handed to the probe root, which must build its decks inside its own scope.
        static PROBE_AUDIO: RefCell<Option<SharedAudio>> = const { RefCell::new(None) };
        /// Handed back, so a test can drive the very decks the probe rendered.
        static PROBE_DECKS: RefCell<Option<[Deck; DECK_COUNT]>> = const { RefCell::new(None) };
    }

    /// The DJ view's deck row, reduced to the parts that show playback.
    #[component]
    fn DeckRow() -> Element {
        let decks = use_hook(|| {
            let shared = PROBE_AUDIO
                .with(|slot| slot.borrow().clone())
                .expect("no probe audio was staged");
            let audio = Signal::new(shared);
            let decks: [Deck; DECK_COUNT] = std::array::from_fn(|index| Deck::new(audio, index));
            PROBE_DECKS.with(|slot| *slot.borrow_mut() = Some(decks));
            decks
        });

        rsx! {
            for deck in decks {
                ui::DeckControl {
                    deck_number: deck.number(),
                    state: deck.state(),
                    track: deck.track(),
                    position: deck.position(),
                    volume: deck.volume(),
                    on_play: move |_| deck.play(),
                    on_pause: move |_| deck.pause(),
                    on_stop: move |_| deck.stop(),
                    on_volume_change: move |volume| deck.set_volume(volume),
                    on_seek: move |position| deck.seek(position),
                    on_file_drop: |_| {},
                }
            }
        }
    }

    /// Where deck one's playhead is drawn, in milliseconds, as the markup reports it.
    fn rendered_playhead(dom: &VirtualDom) -> u64 {
        let html = dioxus_ssr::render(dom);
        let slider = html
            .split("<input")
            .find(|tag| tag.contains("trackbar-slider"))
            .and_then(|tag| tag.split('>').next())
            .unwrap_or_else(|| panic!("no seek slider was rendered:\n{html}"));
        slider
            .split("value=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_else(|| panic!("the seek slider has no value:\n{slider}"))
            .parse()
            .unwrap()
    }

    fn a_four_minute_track() -> LoadedTrack {
        let mut track = Track::new_without_metadata(PathBuf::new());
        track.title = "Playing".into();
        track.set_duration(Duration::from_secs(240));
        LoadedTrack {
            track,
            audio: AudioInput::File(PathBuf::from("/cache/playing.mp3")),
            cached: true,
            streamed_from: None,
        }
    }

    /// The DJ view's deck row *plus* the refresh loop that feeds it, as the running
    /// application wires them together.
    #[component]
    fn RefreshingDeckRow() -> Element {
        let decks = use_hook(|| {
            let shared = PROBE_AUDIO
                .with(|slot| slot.borrow().clone())
                .expect("no probe audio was staged");
            let audio = Signal::new(shared);
            let decks: [Deck; DECK_COUNT] = std::array::from_fn(|index| Deck::new(audio, index));
            PROBE_DECKS.with(|slot| *slot.borrow_mut() = Some(decks));
            decks
        });

        // The application's one refresh loop, at a pace a test can wait for.
        use_future(move || async move {
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                for deck in decks {
                    deck.refresh();
                }
            }
        });

        rsx! {
            for deck in decks {
                ui::DeckControl {
                    deck_number: deck.number(),
                    state: deck.state(),
                    track: deck.track(),
                    position: deck.position(),
                    volume: deck.volume(),
                    on_play: move |_| deck.play(),
                    on_pause: move |_| deck.pause(),
                    on_stop: move |_| deck.stop(),
                    on_volume_change: move |volume| deck.set_volume(volume),
                    on_seek: move |position| deck.seek(position),
                    on_file_drop: |_| {},
                }
            }
        }
    }

    /// The refresh loop is the only thing that moves a playhead while a track plays.
    /// Nothing else in the application polls the audio output.
    #[tokio::test]
    async fn the_refresh_loop_carries_the_playhead_to_the_view() {
        let fake = Arc::new(StdMutex::new(FakeAudio::new()));
        PROBE_AUDIO.with(|slot| *slot.borrow_mut() = Some(fake.clone()));

        let mut dom = VirtualDom::new(RefreshingDeckRow);
        dom.rebuild_in_place();
        let decks = PROBE_DECKS
            .with(|slot| slot.borrow_mut().take())
            .expect("the probe root did not publish its decks");
        dom.in_runtime(|| {
            decks[0].load(a_four_minute_track());
            decks[0].play();
        });
        dom.render_immediate(&mut dioxus_core::NoOpMutations);

        // The audio output moves on, and nothing tells the view about it but the loop.
        fake.lock().unwrap().decks[0].position = Duration::from_secs(30);
        for _ in 0..40 {
            let _ = tokio::time::timeout(Duration::from_millis(25), dom.wait_for_work()).await;
            dom.render_immediate(&mut dioxus_core::NoOpMutations);
            if rendered_playhead(&dom) == 30_000 {
                return;
            }
        }

        panic!(
            "the refresh loop never carried the playhead to the view; it is stuck at {}ms",
            rendered_playhead(&dom)
        );
    }

    /// The user's symptom: the audio moves, the trackbar does not.
    #[test]
    fn a_moving_playhead_moves_the_trackbar() {
        let fake = Arc::new(StdMutex::new(FakeAudio::new()));
        PROBE_AUDIO.with(|slot| *slot.borrow_mut() = Some(fake.clone()));

        let mut dom = VirtualDom::new(DeckRow);
        dom.rebuild_in_place();
        let decks = PROBE_DECKS
            .with(|slot| slot.borrow_mut().take())
            .expect("the probe root did not publish its decks");

        dom.in_runtime(|| {
            decks[0].load(a_four_minute_track());
            decks[0].play();
        });
        dom.render_immediate(&mut dioxus_core::NoOpMutations);
        assert_eq!(
            rendered_playhead(&dom),
            0,
            "playback starts at the beginning"
        );

        // The audio output has moved on, exactly as it does while a track plays.
        fake.lock().unwrap().decks[0].position = Duration::from_secs(30);
        dom.in_runtime(|| decks[0].refresh());
        dom.render_immediate(&mut dioxus_core::NoOpMutations);

        assert_eq!(
            rendered_playhead(&dom),
            30_000,
            "the audio moved but the trackbar did not follow it"
        );
    }
}
