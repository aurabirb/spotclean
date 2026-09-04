//! The "Sort" tab's state: which playlists have a key bound to them, and the queue of
//! file-this-track-away playlist mutations.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, RwLock};

use log::warn;

use crate::commands::CommandManager;
use crate::config::Config;
use crate::library::Library;
use crate::model::playable::Playable;
use crate::model::playlist::Playlist;
use crate::spotify::Spotify;
use crate::traits::ListItem;
use crate::worker_queue::spawn_queue_worker;

/// Whether a track has been filed into a keybind-bound playlist. See [`SortState::sort_status`].
pub enum SortStatus {
    /// No playlist is currently bound to a key, so the concept doesn't apply.
    Unavailable,
    /// Not filed into any bound playlist yet.
    Unsorted,
    /// Filed into the playlist(s) bound to these keys. A track can be filed into more than one
    /// bound playlist at once, so this holds every matching key, not just the first found.
    Sorted(Vec<String>),
}

/// A pending playlist change queued by [`SortState::queue_playlist_mutation`]. Playlists are
/// referenced by id rather than by the (possibly already stale) snapshot the caller had in hand,
/// since the mutation may sit in the queue behind others that touch the same playlist - by the
/// time it actually runs, the worker refetches the current cached copy so it never clobbers a
/// change made by a mutation that was queued earlier but is still ahead of it.
pub struct PlaylistMutation {
    /// Ids of playlists to remove `track` from (already known to contain it).
    remove_from: Vec<String>,
    /// Id of the playlist to append `track` to, if given and it's not already in it.
    append_to: Option<String>,
    track: crate::model::track::Track,
}

/// Owns the bound-playlist-key cache and the playlist-mutation queue.
#[derive(Clone)]
pub struct SortState {
    /// Cached result of [`CommandManager::bound_playlist_keys`], keyed by lowercased playlist
    /// name. [`SortState::sort_status`] is called once per visible row on every redraw, so it
    /// reads this cache instead of re-parsing the keybinding config (and logging every custom
    /// binding) on every row of every frame - see [`SortState::refresh`].
    bound_playlist_keys: Arc<RwLock<HashMap<String, String>>>,
    /// Queue of playlist add/remove operations, drained one at a time by a single background
    /// worker thread instead of spawning a new thread per keypress.
    playlist_mutation_queue: Sender<PlaylistMutation>,
}

impl SortState {
    /// Create the state and the receiving half of its mutation queue (feed the latter to
    /// [`SortState::spawn_worker`] once a [`Library`] handle exists).
    pub fn create() -> (Self, mpsc::Receiver<PlaylistMutation>) {
        let (playlist_mutation_queue, rx) = mpsc::channel();
        (
            Self {
                bound_playlist_keys: Arc::new(RwLock::new(HashMap::new())),
                playlist_mutation_queue,
            },
            rx,
        )
    }

    /// Spawn the single background worker that drains the playlist mutation queue, one operation
    /// at a time, instead of spawning a new thread per keypress.
    pub fn spawn_worker(library: Library, rx: mpsc::Receiver<PlaylistMutation>) {
        spawn_queue_worker("playlist-worker", rx, move |mutation| {
            apply_playlist_mutation(&library, mutation);
        });
    }

    /// Recompute the cached bound-playlist-key map from the current config. Call this whenever
    /// keybindings change (config reload, or a key bound/unbound) - never from a per-row/per-draw
    /// path, since it re-parses the keybinding config and logs every custom binding.
    pub fn refresh(&self, cfg: &Config) {
        *self.bound_playlist_keys.write().unwrap() = CommandManager::bound_playlist_keys(cfg);
    }

    /// Names (lowercased) of playlists that currently have a key bound to them.
    pub fn bound_playlist_names(&self) -> HashSet<String> {
        self.bound_playlist_keys
            .read()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    /// Whether `track_id` has already been filed into a keybind-bound playlist, and which key(s)
    /// it's filed under if so.
    pub fn sort_status(
        &self,
        playlists: &RwLock<Vec<Playlist>>,
        track_id: &str,
    ) -> SortStatus {
        let bound_keys = self.bound_playlist_keys.read().unwrap();
        if bound_keys.is_empty() {
            return SortStatus::Unavailable;
        }

        let keys: Vec<String> = playlists
            .read()
            .unwrap()
            .iter()
            .filter_map(|p| {
                let key = bound_keys.get(&p.name.to_ascii_lowercase())?;
                p.has_track(track_id).then(|| key.clone())
            })
            .collect();

        if keys.is_empty() {
            SortStatus::Unsorted
        } else {
            SortStatus::Sorted(keys)
        }
    }

    /// Queue `track` to be moved into `append_to`, dropping it from `remove_from` first (any other
    /// keybind-bound playlist it's currently in). Liked Songs is never touched. Returns
    /// immediately; a single background worker thread applies mutations one at a time so filing
    /// away tracks quickly never spawns a pile of concurrent API calls. Pass `None` for
    /// `append_to` to just remove the track from `remove_from` without filing it anywhere else.
    pub fn queue_playlist_mutation(
        &self,
        track: crate::model::track::Track,
        remove_from: Vec<Playlist>,
        append_to: Option<Playlist>,
    ) {
        let _ = self.playlist_mutation_queue.send(PlaylistMutation {
            remove_from: remove_from.into_iter().map(|p| p.id).collect(),
            append_to: append_to.map(|p| p.id),
            track,
        });
    }
}

/// Remove every occurrence of `track_id` from `playlist` in a single batch call, not just the
/// first one found. A playlist can contain the same track more than once (added by hand,
/// imported, etc.), and leaving duplicates behind after "removing" a track is surprising.
pub(crate) fn delete_all_occurrences(
    playlist: &mut Playlist,
    track_id: &str,
    spotify: Spotify,
    library: &Library,
) -> bool {
    let Some(tracks) = playlist.tracks.as_ref() else {
        return false;
    };

    let occurrences: Vec<Playable> = tracks
        .iter()
        .filter(|t| t.id().as_deref() == Some(track_id))
        .cloned()
        .collect();

    if occurrences.is_empty() {
        return false;
    }

    if occurrences
        .iter()
        .any(|t| t.track().map(|t| t.is_local) == Some(true))
    {
        warn!("track is a local file, can't delete");
        return false;
    }

    match spotify
        .api
        .delete_tracks(&playlist.id, &playlist.snapshot_id, &occurrences)
        .is_ok()
    {
        false => false,
        true => {
            if let Some(tracks) = &mut playlist.tracks {
                tracks.retain(|t| t.id().as_deref() != Some(track_id));
                library.playlist_update(playlist);
            }

            true
        }
    }
}

/// The current cached copy of the playlist with the given id, if any. Used by
/// [`apply_playlist_mutation`] to always act on the latest known state instead of whatever
/// snapshot was captured when the mutation was originally queued.
fn playlist_by_id(library: &Library, id: &str) -> Option<Playlist> {
    library
        .playlists
        .read()
        .unwrap()
        .iter()
        .find(|p| p.id == id)
        .cloned()
}

/// Apply a queued [`PlaylistMutation`]. Runs on the playlist worker thread. Playlists are looked
/// up fresh from the cache here (rather than using a snapshot captured back when the mutation was
/// queued) so a mutation that's been waiting behind others in the queue doesn't clobber their
/// changes with stale data once it finally runs.
fn apply_playlist_mutation(library: &Library, mutation: PlaylistMutation) {
    let PlaylistMutation {
        remove_from,
        append_to,
        track,
    } = mutation;
    let track_id = track.id.clone();

    // Append to the new playlist before removing from the old one: if the append fails (network
    // hiccup, transient API error), the track keeps its old key instead of ending up filed under
    // none at all.
    let appended = match append_to
        .as_deref()
        .and_then(|id| playlist_by_id(library, id))
    {
        Some(mut append_to) => {
            if append_to.has_track(track_id.as_deref().unwrap_or_default()) {
                true
            } else {
                append_to.append_tracks(&[Playable::Track(track)], library.spotify(), library)
            }
        }
        None => append_to.is_none(),
    };

    if !appended {
        return;
    }

    if let Some(track_id) = track_id.as_deref() {
        for id in remove_from {
            if let Some(mut other) = playlist_by_id(library, &id) {
                delete_all_occurrences(&mut other, track_id, library.spotify().clone(), library);
            }
        }
    }
}
