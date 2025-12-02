//! Fuzzy search engine for tracks using nucleo-matcher.
//!
//! Provides fast fuzzy matching across track metadata fields.

use crate::Track;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Search result with track and match score.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// The matching track
    pub track: Track,
    /// Match score (higher is better)
    pub score: u32,
}

/// Fuzzy search engine for tracks.
pub struct SearchEngine {
    matcher: Matcher,
}

impl Default for SearchEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchEngine {
    /// Create a new search engine.
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    /// Search tracks with a fuzzy query.
    ///
    /// Returns tracks that match the query, sorted by relevance (best first).
    /// Searches across title, artist, album, and genre fields.
    pub fn search(&mut self, query: &str, tracks: &[Track]) -> Vec<SearchResult> {
        if query.is_empty() {
            return tracks
                .iter()
                .cloned()
                .map(|track| SearchResult { track, score: 0 })
                .collect();
        }

        let pattern = Pattern::new(
            query,
            CaseMatching::Ignore,
            Normalization::Smart,
            nucleo_matcher::pattern::AtomKind::Fuzzy,
        );

        let mut results: Vec<SearchResult> = tracks
            .iter()
            .filter_map(|track| {
                let score = self.match_track(&pattern, track);
                if score > 0 {
                    Some(SearchResult {
                        track: track.clone(),
                        score,
                    })
                } else {
                    None
                }
            })
            .collect();

        // Sort by score descending (best matches first)
        results.sort_by(|a, b| b.score.cmp(&a.score));
        results
    }

    /// Match a track against a pattern, returning the best score across all fields.
    fn match_track(&mut self, pattern: &Pattern, track: &Track) -> u32 {
        let mut best_score = 0u32;

        // Title has highest weight (3x)
        if let Some(score) = self.match_field(pattern, &track.title) {
            best_score = best_score.max(score.saturating_mul(3));
        }

        // Artist has high weight (2x)
        if let Some(ref artist) = track.artist {
            if let Some(score) = self.match_field(pattern, artist) {
                best_score = best_score.max(score.saturating_mul(2));
            }
        }

        // Album has normal weight (1x)
        if let Some(ref album) = track.album {
            if let Some(score) = self.match_field(pattern, album) {
                best_score = best_score.max(score);
            }
        }

        // Genre has lower weight (1x)
        if let Some(ref genre) = track.genre {
            if let Some(score) = self.match_field(pattern, genre) {
                best_score = best_score.max(score);
            }
        }

        best_score
    }

    /// Match a single field against the pattern.
    fn match_field(&mut self, pattern: &Pattern, field: &str) -> Option<u32> {
        let mut buf = Vec::new();
        let haystack = Utf32Str::new(field, &mut buf);
        pattern.score(haystack, &mut self.matcher)
    }

    /// Quick check if a track matches the query (for filtering).
    pub fn matches(&mut self, query: &str, track: &Track) -> bool {
        if query.is_empty() {
            return true;
        }

        let pattern = Pattern::new(
            query,
            CaseMatching::Ignore,
            Normalization::Smart,
            nucleo_matcher::pattern::AtomKind::Fuzzy,
        );

        self.match_track(&pattern, track) > 0
    }
}

/// Convenience function to search tracks without managing SearchEngine lifetime.
pub fn search_tracks(query: &str, tracks: &[Track]) -> Vec<SearchResult> {
    let mut engine = SearchEngine::new();
    engine.search(query, tracks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_track(title: &str, artist: Option<&str>, album: Option<&str>) -> Track {
        let mut track = Track::new_without_metadata(PathBuf::from("/test.mp3"));
        track.title = title.to_string();
        track.artist = artist.map(String::from);
        track.album = album.map(String::from);
        track
    }

    #[test]
    fn test_exact_title_match() {
        let tracks = vec![
            make_track("Hello World", Some("Artist A"), None),
            make_track("Goodbye", Some("Artist B"), None),
        ];

        let results = search_tracks("hello", &tracks);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].track.title, "Hello World");
    }

    #[test]
    fn test_fuzzy_match() {
        let tracks = vec![
            make_track("Electronic Dreams", Some("Synthwave"), None),
            make_track("Rock Anthem", Some("The Band"), None),
        ];

        let results = search_tracks("elec dre", &tracks);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].track.title, "Electronic Dreams");
    }

    #[test]
    fn test_artist_match() {
        let tracks = vec![
            make_track("Song One", Some("Daft Punk"), None),
            make_track("Song Two", Some("The Beatles"), None),
        ];

        let results = search_tracks("daft", &tracks);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].track.artist, Some("Daft Punk".to_string()));
    }

    #[test]
    fn test_empty_query_returns_all() {
        let tracks = vec![
            make_track("Track 1", None, None),
            make_track("Track 2", None, None),
        ];

        let results = search_tracks("", &tracks);
        assert_eq!(results.len(), 2);
    }
}

