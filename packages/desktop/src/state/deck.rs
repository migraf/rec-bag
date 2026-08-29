//! One playback channel that a Loaded Track occupies.
//!
//! Views ask a deck to play, pause, seek, or load. They never lock the audio engine,
//! never name a deck by a literal index, and never reconcile display state by hand:
//! the deck owns the lock, the index, and the reconciliation, so playback behaves the
//! same wherever it is driven from.

use dioxus::prelude::*;
use rec_core::{AudioEngine, DeckState, Track};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use ui::DeckDisplayState;
use uuid::Uuid;

/// How many decks the application runs.
pub const DECK_COUNT: usize = 2;

/// What a deck needs from audio output, independent of any audio device.
///
/// Two adapters satisfy it: [`AudioEngine`] in the running application and an
/// in-memory fake in this module's tests. Every method takes the deck's own index, so
/// an out-of-range index is this trait's problem and never a caller's.
pub trait DeckAudio {
    fn load(&mut self, index: usize, track: Track) -> bool;
    fn play(&mut self, index: usize);
    fn pause(&mut self, index: usize);
    fn stop(&mut self, index: usize);
    fn seek(&mut self, index: usize, position: Duration);
    fn set_volume(&mut self, index: usize, volume: f32);
    fn state(&self, index: usize) -> DeckState;
    fn position(&self, index: usize) -> Duration;
    fn volume(&self, index: usize) -> f32;
    fn is_finished(&self, index: usize) -> bool;
}

impl DeckAudio for AudioEngine {
    fn load(&mut self, index: usize, track: Track) -> bool {
        self.load_track(index, track).is_ok()
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

    fn is_finished(&self, index: usize) -> bool {
        self.deck(index)
            .map(|deck| deck.is_finished())
            .unwrap_or(false)
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
}

impl Default for DeckSnapshot {
    fn default() -> Self {
        Self {
            state: DeckDisplayState::Empty,
            position: Duration::ZERO,
            volume: 1.0,
        }
    }
}

/// One playback channel that a Loaded Track occupies.
#[derive(Clone, Copy)]
pub struct Deck {
    audio: Signal<SharedAudio>,
    index: usize,
    track: Signal<Option<Track>>,
    live: Signal<DeckSnapshot>,
}

impl Deck {
    /// Open deck `index` on the given audio output. Must run inside a Dioxus scope.
    pub fn new(audio: Signal<SharedAudio>, index: usize) -> Self {
        Self {
            audio,
            index,
            track: Signal::new(None),
            live: Signal::new(DeckSnapshot::default()),
        }
    }

    /// The deck's user-facing number, counting from one.
    pub fn number(&self) -> usize {
        self.index + 1
    }

    /// The Loaded Track, if this deck holds one.
    pub fn track(&self) -> Option<Track> {
        self.track.read().clone()
    }

    /// Whether this deck holds the given canonical track.
    pub fn holds(&self, track_id: Uuid) -> bool {
        self.track
            .read()
            .as_ref()
            .is_some_and(|track| track.id == track_id)
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

    pub fn is_playing(&self) -> bool {
        self.state() == DeckDisplayState::Playing
    }

    /// Put `track` on this deck, decoding audio from `playback_path`.
    ///
    /// The path is only what the audio engine reads from; the deck keeps the track's
    /// own identity, so a cached copy of a remote asset never becomes the track.
    pub fn load(&self, track: Track, playback_path: std::path::PathBuf) -> bool {
        let mut decoded = track.clone();
        decoded.path = playback_path;
        if !self.with_audio(|audio, index| audio.load(index, decoded)) {
            return false;
        }
        self.set_track(Some(track));
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

    /// Apply `edit` to the Loaded Track when this deck holds `track_id`.
    ///
    /// Analysis results and metadata edits reach a playing track through here, so
    /// every deck stays current without any caller enumerating the decks by hand.
    pub fn edit_track(&self, track_id: Uuid, edit: impl FnOnce(&mut Track)) -> bool {
        if !self.holds(track_id) {
            return false;
        }
        let mut track = match self.track.read().clone() {
            Some(track) => track,
            None => return false,
        };
        edit(&mut track);
        self.set_track(Some(track));
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

    fn set_track(&self, track: Option<Track>) {
        let mut slot = self.track;
        slot.set(track);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// An audio output that records what it was asked to do, so deck behaviour can be
    /// tested without an audio device.
    #[derive(Default)]
    struct FakeAudio {
        decks: Vec<FakeDeck>,
        calls: Vec<String>,
    }

    #[derive(Clone)]
    struct FakeDeck {
        state: DeckState,
        position: Duration,
        volume: f32,
        finished: bool,
        refuse_load: bool,
        loaded_from: Option<PathBuf>,
    }

    impl Default for FakeDeck {
        fn default() -> Self {
            Self {
                state: DeckState::Empty,
                position: Duration::ZERO,
                volume: 1.0,
                finished: false,
                refuse_load: false,
                loaded_from: None,
            }
        }
    }

    impl FakeAudio {
        fn new() -> Self {
            Self {
                decks: vec![FakeDeck::default(); DECK_COUNT],
                calls: Vec::new(),
            }
        }
    }

    impl DeckAudio for FakeAudio {
        fn load(&mut self, index: usize, track: Track) -> bool {
            self.calls.push(format!("load {index}"));
            if self.decks[index].refuse_load {
                return false;
            }
            self.decks[index].loaded_from = Some(track.path);
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
        fn state(&self, index: usize) -> DeckState {
            self.decks[index].state
        }
        fn position(&self, index: usize) -> Duration {
            self.decks[index].position
        }
        fn volume(&self, index: usize) -> f32 {
            self.decks[index].volume
        }
        fn is_finished(&self, index: usize) -> bool {
            self.decks[index].finished
        }
    }

    /// Decks need Dioxus signals, which need a scope to be owned by.
    fn with_decks(test: impl FnOnce([Deck; DECK_COUNT], Arc<StdMutex<FakeAudio>>)) {
        fn app() -> Element {
            rsx! { "" }
        }
        let mut dom = VirtualDom::new(app);
        dom.rebuild_in_place();
        dom.in_runtime(|| {
            let fake = Arc::new(StdMutex::new(FakeAudio::new()));
            let shared: SharedAudio = fake.clone();
            let audio = Signal::new_in_scope(shared, ScopeId::ROOT);
            let decks = [deck_in_scope(audio, 0), deck_in_scope(audio, 1)];
            test(decks, fake);
        });
    }

    fn deck_in_scope(audio: Signal<SharedAudio>, index: usize) -> Deck {
        Deck {
            audio,
            index,
            track: Signal::new_in_scope(None, ScopeId::ROOT),
            live: Signal::new_in_scope(DeckSnapshot::default(), ScopeId::ROOT),
        }
    }

    fn a_track() -> Track {
        Track::new(PathBuf::from("/music/canonical.mp3"))
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
            let track = a_track();
            assert!(decks[0].load(track.clone(), PathBuf::from("/cache/drive/abc.mp3")));
            assert_eq!(
                fake.lock().unwrap().decks[0].loaded_from,
                Some(PathBuf::from("/cache/drive/abc.mp3"))
            );
            assert_eq!(
                decks[0].track().map(|t| t.path),
                Some(PathBuf::from("/music/canonical.mp3"))
            );
        });
    }

    #[test]
    fn a_deck_that_cannot_decode_does_not_claim_the_track() {
        with_decks(|decks, fake| {
            fake.lock().unwrap().decks[0].refuse_load = true;
            assert!(!decks[0].load(a_track(), PathBuf::from("/cache/x.mp3")));
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
            let track = a_track();
            let id = track.id;
            decks[0].load(track, PathBuf::from("/cache/a.mp3"));
            decks[1].load(a_track(), PathBuf::from("/cache/b.mp3"));

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
}
