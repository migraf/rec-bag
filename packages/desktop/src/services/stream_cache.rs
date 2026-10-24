//! Streamed audio kept on disk for the rest of the session.
//!
//! The SoundCloud API Terms allow caching a stream only for the session it was played in
//! (section 5): no saved files and no offline access. Each entry is therefore an anonymous
//! temporary file. It has no name in the filesystem, so nothing outside the app can open it,
//! and the operating system deletes it as soon as its handle closes, which includes the app
//! exiting, crashing or being killed. Nothing has to be cleaned up at exit or on the next
//! start, and no entry can outlive the process that fetched it.

use std::{
    collections::HashMap,
    fs::File,
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use uuid::Uuid;

/// Disk the cache may use before the least recently played streams are dropped.
const MAX_BYTES: u64 = 2 << 30;
/// Each entry holds an open file handle, so the count is bounded well below process limits.
const MAX_ENTRIES: usize = 256;

/// A stream read back from the session cache.
pub struct CachedStream {
    pub bytes: Arc<[u8]>,
    pub preview: bool,
}

/// Which provider changes have happened, so a stream fetched before its source was forgotten
/// is not cached after it. See [`SessionStreamCache::generation`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Generation(u64);

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    source_id: Uuid,
    external_id: String,
}

struct Entry {
    file: Arc<File>,
    len: usize,
    preview: bool,
    last_used: u64,
}

#[derive(Default)]
struct State {
    entries: HashMap<Key, Entry>,
    bytes: u64,
    clock: u64,
    generation: u64,
}

/// Streams fetched this session, by the provider track they came from.
///
/// Caching is best effort: a stream that cannot be written or read back is simply fetched
/// again.
pub struct SessionStreamCache {
    directory: PathBuf,
    max_bytes: u64,
    max_entries: usize,
    state: Mutex<State>,
}

impl SessionStreamCache {
    /// A cache whose anonymous files are allocated on the filesystem holding `directory`.
    pub fn new(directory: PathBuf) -> Self {
        Self::with_limits(directory, MAX_BYTES, MAX_ENTRIES)
    }

    fn with_limits(directory: PathBuf, max_bytes: u64, max_entries: usize) -> Self {
        Self {
            directory,
            max_bytes,
            max_entries,
            state: Mutex::new(State::default()),
        }
    }

    /// Taken before fetching a stream and handed back to [`Self::insert`].
    pub fn generation(&self) -> Generation {
        Generation(self.state.lock().unwrap().generation)
    }

    pub async fn get(&self, source_id: Uuid, external_id: &str) -> Option<CachedStream> {
        let key = Key {
            source_id,
            external_id: external_id.to_string(),
        };
        let (file, len, preview) = {
            let mut state = self.state.lock().unwrap();
            state.clock += 1;
            let clock = state.clock;
            let entry = state.entries.get_mut(&key)?;
            entry.last_used = clock;
            (entry.file.clone(), entry.len, entry.preview)
        };
        match tokio::task::spawn_blocking(move || read_all(&file, len)).await {
            Ok(Ok(bytes)) => Some(CachedStream {
                bytes: bytes.into(),
                preview,
            }),
            failed => {
                tracing::warn!(?failed, "could not read a cached stream; fetching it again");
                self.remove(&key);
                None
            }
        }
    }

    /// Keeps `bytes` for the rest of the session, unless a source was forgotten since
    /// `generation` was taken.
    pub async fn insert(
        &self,
        generation: Generation,
        source_id: Uuid,
        external_id: &str,
        bytes: Arc<[u8]>,
        preview: bool,
    ) {
        if bytes.len() as u64 > self.max_bytes {
            return;
        }
        let directory = self.directory.clone();
        let len = bytes.len();
        let written = tokio::task::spawn_blocking(move || -> io::Result<File> {
            std::fs::create_dir_all(&directory)?;
            let mut file = tempfile::tempfile_in(&directory)?;
            file.write_all(&bytes)?;
            Ok(file)
        })
        .await;
        let file = match written {
            Ok(Ok(file)) => Arc::new(file),
            failed => {
                tracing::warn!(?failed, "could not cache a stream for this session");
                return;
            }
        };
        let mut state = self.state.lock().unwrap();
        if state.generation != generation.0 {
            return;
        }
        state.clock += 1;
        let entry = Entry {
            file,
            len,
            preview,
            last_used: state.clock,
        };
        let key = Key {
            source_id,
            external_id: external_id.to_string(),
        };
        if let Some(replaced) = state.entries.insert(key, entry) {
            state.bytes -= replaced.len as u64;
        }
        state.bytes += len as u64;
        while state.bytes > self.max_bytes || state.entries.len() > self.max_entries {
            let oldest = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            let Some(evicted) = oldest.and_then(|key| state.entries.remove(&key)) else {
                break;
            };
            state.bytes -= evicted.len as u64;
        }
    }

    /// Drops every stream from `source_id`, and any of its streams still being fetched, as
    /// when the user disconnects it.
    pub fn forget_source(&self, source_id: Uuid) {
        let mut state = self.state.lock().unwrap();
        state.generation += 1;
        let State { entries, bytes, .. } = &mut *state;
        entries.retain(|key, entry| {
            let keep = key.source_id != source_id;
            if !keep {
                *bytes -= entry.len as u64;
            }
            keep
        });
    }

    fn remove(&self, key: &Key) {
        let mut state = self.state.lock().unwrap();
        if let Some(removed) = state.entries.remove(key) {
            state.bytes -= removed.len as u64;
        }
    }
}

/// Reads a whole entry without moving a shared file cursor, so concurrent reads are safe.
fn read_all(file: &File, len: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; len];
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(&mut bytes, 0)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut read = 0;
        while read < len {
            match file.seek_read(&mut bytes[read..], read as u64)? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => read += n,
            }
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(fill: u8, len: usize) -> Arc<[u8]> {
        vec![fill; len].into()
    }

    #[tokio::test]
    async fn a_cached_stream_is_read_back_without_leaving_a_file_anyone_could_open() {
        let directory = tempfile::tempdir().unwrap();
        let cache = SessionStreamCache::new(directory.path().to_path_buf());
        let source = Uuid::new_v4();

        cache
            .insert(
                cache.generation(),
                source,
                "soundcloud:tracks:1",
                audio(7, 4096),
                true,
            )
            .await;

        let cached = cache.get(source, "soundcloud:tracks:1").await.unwrap();
        assert_eq!(&*cached.bytes, &*audio(7, 4096));
        assert!(cached.preview);
        assert!(cache.get(source, "soundcloud:tracks:2").await.is_none());
        #[cfg(unix)]
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn the_least_recently_played_streams_make_room_for_new_ones() {
        let directory = tempfile::tempdir().unwrap();
        let cache = SessionStreamCache::with_limits(directory.path().to_path_buf(), 300, 10);
        let source = Uuid::new_v4();
        for id in ["a", "b", "c"] {
            cache
                .insert(cache.generation(), source, id, audio(1, 100), false)
                .await;
        }

        cache.get(source, "a").await.unwrap();
        cache
            .insert(cache.generation(), source, "d", audio(2, 100), false)
            .await;

        assert!(cache.get(source, "b").await.is_none());
        for id in ["a", "c", "d"] {
            assert!(cache.get(source, id).await.is_some(), "{id} was evicted");
        }
    }

    #[tokio::test]
    async fn a_forgotten_source_keeps_nothing_not_even_a_stream_it_was_still_fetching() {
        let directory = tempfile::tempdir().unwrap();
        let cache = SessionStreamCache::new(directory.path().to_path_buf());
        let (forgotten, kept) = (Uuid::new_v4(), Uuid::new_v4());
        cache
            .insert(cache.generation(), forgotten, "1", audio(1, 10), false)
            .await;
        cache
            .insert(cache.generation(), kept, "1", audio(2, 10), false)
            .await;
        let fetching = cache.generation();

        cache.forget_source(forgotten);
        cache
            .insert(fetching, forgotten, "2", audio(3, 10), false)
            .await;

        assert!(cache.get(forgotten, "1").await.is_none());
        assert!(cache.get(forgotten, "2").await.is_none());
        assert!(cache.get(kept, "1").await.is_some());
    }
}
