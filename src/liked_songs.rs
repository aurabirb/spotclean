//! The user's "Liked Songs" list, virtualized.
//!
//! The list is seeded instantly from the on-disk [`Store`] on launch and revalidated against the
//! API page-by-page as the Tracks/Sort tabs are scrolled, so a huge library never blocks startup
//! and never loads in full unless the user scrolls the whole way down.

use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use log::debug;

use crate::config;
use crate::events::EventManager;
use crate::model::track::Track;
use crate::spotify::Spotify;
use crate::store::Store;

/// Filename of the [`redb`](crate::store) store holding the Liked Songs list and detected BPMs.
const STORE_FILE: &str = "ncspot.redb";

/// Rows past `want` to keep validated ahead of the scroll position - the page loader fetches this
/// far beyond what the view has asked for, so scrolling doesn't repeatedly stall waiting a page.
const SAVED_TRACKS_MARGIN: usize = 150;

/// Open the shared on-disk store and tidy up caches that predate it.
pub fn open_store() -> Store {
    let store = Store::open(&config::cache_path(STORE_FILE));
    // The saved-track list and BPMs used to live in these JSON caches; they moved into the redb
    // store. Tidy up so they don't linger.
    for stale in ["tracks.db", "bpm.db"] {
        let _ = std::fs::remove_file(config::cache_path(stale));
    }
    store
}

#[derive(Default)]
struct SavedTracksMeta {
    /// Total number of liked songs reported by the API, once the first page has come back.
    total: Option<usize>,
    /// Rows `[0, fetched_through)` of the list have been confirmed against the API this session
    /// (or trusted wholesale from an unchanged cache - see `fresh`).
    fetched_through: usize,
    /// The cached list matched the API on the last check, so the whole thing is trusted and no
    /// scrolling triggers a fetch.
    fresh: bool,
}

struct Inner {
    spotify: Spotify,
    ev: EventManager,
    store: Store,
    /// The user's "Liked Songs" in API order (newest first).
    saved_tracks: Arc<RwLock<Vec<Track>>>,
    meta: RwLock<SavedTracksMeta>,
    /// Bumped on every change to `saved_tracks` so views (e.g. the Sort tab's filtered snapshot)
    /// can tell when they need to rebuild.
    version: RwLock<u64>,
    /// Held while fetching saved-track pages so scroll events never kick off overlapping walks.
    fetch_lock: Mutex<()>,
}

/// Owns the Liked Songs list and its incremental revalidation. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct LikedSongs {
    inner: Arc<Inner>,
}

impl LikedSongs {
    pub fn new(spotify: Spotify, ev: EventManager, store: Store) -> Self {
        Self {
            inner: Arc::new(Inner {
                spotify,
                ev,
                store,
                saved_tracks: Arc::new(RwLock::new(Vec::new())),
                meta: RwLock::new(SavedTracksMeta::default()),
                version: RwLock::new(0),
                fetch_lock: Mutex::new(()),
            }),
        }
    }

    /// The shared handle to the list, exposed as `Library::saved_tracks`.
    pub fn saved_tracks(&self) -> Arc<RwLock<Vec<Track>>> {
        self.inner.saved_tracks.clone()
    }

    /// Current version counter for the list; changes whenever it is mutated.
    pub fn version(&self) -> u64 {
        *self.inner.version.read().unwrap()
    }

    /// Total number of liked songs the API reports, if the first page has come back yet. Compare
    /// with `saved_tracks.read().len()` to tell whether the whole list is loaded.
    pub fn total(&self) -> Option<usize> {
        self.inner.meta.read().unwrap().total
    }

    fn bump_version(&self) {
        *self.inner.version.write().unwrap() += 1;
    }

    /// Seed the list from the store (instant, no network).
    pub fn load_cache(&self) {
        let cached: Vec<Track> = self.inner.store.load_saved_tracks();
        debug!("loaded {} saved tracks from the store", cached.len());
        *self.inner.saved_tracks.write().unwrap() = cached;
        {
            let mut meta = self.inner.meta.write().unwrap();
            meta.fetched_through = 0;
            meta.total = self.inner.store.saved_total();
        }
        self.bump_version();
    }

    /// Fetch page 0 and reconcile it with the cached list: the first page is always refreshed,
    /// and if the cache still matches the API exactly the whole list is trusted. Otherwise the
    /// stale rows stay on screen and get replaced page-by-page as the user scrolls into them
    /// (see [`LikedSongs::ensure_through`]).
    pub fn revalidate(&self) {
        // Share the fetch lock with `ensure_through` so a view that asks for page 0 at startup
        // waits for this reconciliation instead of racing a second identical request.
        let _guard = self.inner.fetch_lock.lock().unwrap();
        let Ok(page) = self.inner.spotify.api.current_user_saved_tracks(0) else {
            return;
        };
        let total = page.total as usize;
        let head: Vec<Track> = page.items.iter().map(|s| s.into()).collect();

        let head_len = head.len();
        let (new_len, replaced_all) = {
            let mut store = self.inner.saved_tracks.write().unwrap();
            let old_len = store.len();
            let head_matches =
                head_len <= old_len && head.iter().zip(store.iter()).all(|(a, b)| a.id == b.id);
            let replaced_all = old_len <= head_len;

            if replaced_all {
                *store = head;
            } else {
                store.splice(0..head_len, head);
            }
            let new_len = store.len();
            drop(store);

            let mut meta = self.inner.meta.write().unwrap();
            meta.total = Some(total);
            if head_matches && total == old_len {
                meta.fresh = true;
                meta.fetched_through = new_len;
            } else {
                meta.fresh = false;
                meta.fetched_through = new_len.min(total).min(SAVED_TRACKS_MARGIN);
            }
            (new_len, replaced_all)
        };

        // Persist only what changed: the refreshed head, or the whole (short) list.
        let store_rows = self.inner.saved_tracks.read().unwrap();
        let end = if replaced_all { new_len } else { head_len };
        self.inner.store.replace_saved_range(0, &store_rows[..end]);
        drop(store_rows);
        self.inner.store.set_saved_total(total);
        self.bump_version();
        self.inner.ev.trigger();
    }

    /// Ensure rows up to `want` (plus a margin) have been validated against the API, fetching and
    /// splicing in pages as needed. Returns immediately; the work happens on a background thread,
    /// serialized so overlapping scroll events don't launch parallel walks. A no-op once the
    /// whole list is known to be fresh.
    pub fn ensure_through(&self, want: usize) {
        {
            let meta = self.inner.meta.read().unwrap();
            if meta.fresh {
                return;
            }
            if want + SAVED_TRACKS_MARGIN <= meta.fetched_through {
                return;
            }
            if let Some(total) = meta.total
                && meta.fetched_through >= total
            {
                return;
            }
        }

        let liked = self.clone();
        thread::spawn(move || {
            let _guard = liked.inner.fetch_lock.lock().unwrap();
            let mut changed = false;

            loop {
                let (through, total, fresh) = {
                    let meta = liked.inner.meta.read().unwrap();
                    (meta.fetched_through, meta.total, meta.fresh)
                };
                // `revalidate` may have confirmed the whole cache while this thread was waiting on
                // the lock.
                if fresh || want + SAVED_TRACKS_MARGIN <= through {
                    break;
                }
                if let Some(total) = total
                    && through >= total
                {
                    let mut store = liked.inner.saved_tracks.write().unwrap();
                    if store.len() > total {
                        store.truncate(total);
                        drop(store);
                        liked.inner.store.truncate_saved(total as u64);
                        changed = true;
                    }
                    liked.inner.meta.write().unwrap().fresh = true;
                    break;
                }

                let Ok(page) = liked
                    .inner
                    .spotify
                    .api
                    .current_user_saved_tracks(through as u32)
                else {
                    break;
                };
                let items: Vec<Track> = page.items.iter().map(|s| s.into()).collect();
                let got = items.len();

                {
                    let mut store = liked.inner.saved_tracks.write().unwrap();
                    if through >= store.len() {
                        store.extend(items.iter().cloned());
                    } else {
                        let end = (through + got).min(store.len());
                        store.splice(through..end, items.iter().cloned());
                    }
                }
                liked
                    .inner
                    .store
                    .replace_saved_range(through as u64, &items);
                changed = true;

                {
                    let mut meta = liked.inner.meta.write().unwrap();
                    meta.total = Some(page.total as usize);
                    if got == 0 {
                        meta.total = Some(through);
                    } else {
                        meta.fetched_through = through + got;
                    }
                }
                liked.inner.store.set_saved_total(
                    liked.inner.meta.read().unwrap().total.unwrap_or(through),
                );
            }

            if changed {
                liked.bump_version();
                liked.inner.ev.trigger();
            }
        });
    }

    /// Fold newly-liked `tracks` into the front of the list and persist. The API call is the
    /// caller's ([`crate::library::Library::save_tracks`]).
    pub fn apply_saved_added(&self, tracks: &[&Track]) {
        {
            let mut store = self.inner.saved_tracks.write().unwrap();
            for track in tracks.iter().rev() {
                if !store.iter().any(|t| t.id == track.id) {
                    store.insert(0, (*track).clone());
                }
            }
        }
        self.after_local_mutation();
    }

    /// Drop `tracks` from the list and persist. The API call is the caller's
    /// ([`crate::library::Library::unsave_tracks`]).
    pub fn apply_saved_removed(&self, tracks: &[&Track]) {
        {
            let mut store = self.inner.saved_tracks.write().unwrap();
            store.retain(|t| !tracks.iter().any(|tt| t.id == tt.id));
        }
        self.after_local_mutation();
    }

    fn after_local_mutation(&self) {
        let len = self.inner.saved_tracks.read().unwrap().len();
        {
            let mut meta = self.inner.meta.write().unwrap();
            if meta.total.is_some() {
                meta.total = Some(len);
            }
            meta.fetched_through = meta.fetched_through.min(len);
        }
        self.bump_version();
        self.inner
            .store
            .replace_all_saved(&self.inner.saved_tracks.read().unwrap());
        if let Some(total) = self.inner.meta.read().unwrap().total {
            self.inner.store.set_saved_total(total);
        }
    }
}
