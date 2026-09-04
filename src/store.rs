//! Small [`redb`]-backed store for data that would otherwise be rewritten wholesale on every
//! change: the (virtualized) Liked Songs list and locally-detected BPM values. Everything here is
//! best-effort - a failed transaction is logged and swallowed, exactly like the JSON caches in
//! [`crate::library`], because none of it is authoritative (the API is).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use log::{error, warn};
use redb::{Database, ReadableTable, TableDefinition};

use crate::model::track::Track;

/// Boxed so the various `redb` error variants (which are large) don't bloat every `Result`.
type StoreError = Box<dyn std::error::Error + Send + Sync>;

/// Liked Songs by list position (0 = most recently added), value is `serde_json` of [`Track`].
const SAVED: TableDefinition<u64, &str> = TableDefinition::new("saved_tracks");
/// Detected BPM by track id.
const BPM: TableDefinition<&str, f64> = TableDefinition::new("bpm");
/// Miscellaneous scalars, e.g. `"saved_total"`.
const KV: TableDefinition<&str, &str> = TableDefinition::new("kv");

const SAVED_TOTAL_KEY: &str = "saved_total";

#[derive(Clone)]
pub struct Store {
    /// `None` means the store could not be opened; every method then degrades to a no-op.
    db: Option<Arc<Database>>,
}

impl Store {
    /// Open (or create) the store at `path`. If the file is present but unreadable it's removed
    /// and recreated once; if that still fails the store is disabled rather than crashing ncspot.
    pub fn open(path: &Path) -> Self {
        match Database::create(path) {
            Ok(db) => Self {
                db: Some(Arc::new(db)),
            },
            Err(e) => {
                warn!("could not open store at {}: {e}; recreating", path.display());
                let _ = std::fs::remove_file(path);
                match Database::create(path) {
                    Ok(db) => Self {
                        db: Some(Arc::new(db)),
                    },
                    Err(e) => {
                        error!("could not create store at {}: {e}; disabling", path.display());
                        Self { db: None }
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub fn in_memory() -> Self {
        let db = redb::Builder::new()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .expect("in-memory store");
        Self {
            db: Some(Arc::new(db)),
        }
    }

    fn db(&self) -> Option<&Database> {
        self.db.as_deref()
    }

    /// Serialize `tracks` to `(position, json)` rows, logging and skipping any that won't encode.
    fn rows(start: u64, tracks: &[Track]) -> Vec<(u64, String)> {
        tracks
            .iter()
            .enumerate()
            .filter_map(|(i, track)| match serde_json::to_string(track) {
                Ok(json) => Some((start + i as u64, json)),
                Err(e) => {
                    warn!("store: could not serialize track: {e}");
                    None
                }
            })
            .collect()
    }

    /// The saved-track list in position order.
    pub fn load_saved_tracks(&self) -> Vec<Track> {
        match self.try_load_saved_tracks() {
            Ok(tracks) => tracks,
            Err(e) => {
                error!("store: load_saved_tracks failed: {e}");
                Vec::new()
            }
        }
    }

    fn try_load_saved_tracks(&self) -> Result<Vec<Track>, StoreError> {
        let Some(db) = self.db() else {
            return Ok(Vec::new());
        };
        let txn = db.begin_read()?;
        let table = match txn.open_table(SAVED) {
            Ok(t) => t,
            // Table absent on a fresh DB.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for row in table.iter()? {
            let (_, value) = row?;
            match serde_json::from_str::<Track>(value.value()) {
                Ok(track) => out.push(track),
                Err(e) => warn!("store: skipping unparseable saved track: {e}"),
            }
        }
        Ok(out)
    }

    /// Replace the `tracks.len()` positions starting at `start` (leaving later positions alone).
    pub fn replace_saved_range(&self, start: u64, tracks: &[Track]) {
        if let Err(e) = self.try_replace_saved_range(start, tracks) {
            error!("store: replace_saved_range failed: {e}");
        }
    }

    fn try_replace_saved_range(&self, start: u64, tracks: &[Track]) -> Result<(), StoreError> {
        let Some(db) = self.db() else { return Ok(()) };
        let rows = Self::rows(start, tracks);
        let txn = db.begin_write()?;
        {
            let mut table = txn.open_table(SAVED)?;
            for (pos, json) in &rows {
                table.insert(pos, json.as_str())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Rewrite the whole list from `tracks` in a single transaction.
    pub fn replace_all_saved(&self, tracks: &[Track]) {
        if let Err(e) = self.try_replace_all_saved(tracks) {
            error!("store: replace_all_saved failed: {e}");
        }
    }

    fn try_replace_all_saved(&self, tracks: &[Track]) -> Result<(), StoreError> {
        let Some(db) = self.db() else { return Ok(()) };
        let rows = Self::rows(0, tracks);
        let txn = db.begin_write()?;
        txn.delete_table(SAVED)?;
        {
            let mut table = txn.open_table(SAVED)?;
            for (pos, json) in &rows {
                table.insert(pos, json.as_str())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Drop every position `>= len`.
    pub fn truncate_saved(&self, len: u64) {
        if let Err(e) = self.try_truncate_saved(len) {
            error!("store: truncate_saved failed: {e}");
        }
    }

    fn try_truncate_saved(&self, len: u64) -> Result<(), StoreError> {
        let Some(db) = self.db() else { return Ok(()) };
        let txn = db.begin_write()?;
        {
            let mut table = txn.open_table(SAVED)?;
            table.retain(|k, _| k < len)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn saved_total(&self) -> Option<usize> {
        let db = self.db()?;
        let txn = db.begin_read().ok()?;
        let table = txn.open_table(KV).ok()?;
        let raw = table.get(SAVED_TOTAL_KEY).ok()??;
        raw.value().parse().ok()
    }

    pub fn set_saved_total(&self, total: usize) {
        if let Err(e) = self.try_set_kv(SAVED_TOTAL_KEY, &total.to_string()) {
            error!("store: set_saved_total failed: {e}");
        }
    }

    fn try_set_kv(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let Some(db) = self.db() else { return Ok(()) };
        let txn = db.begin_write()?;
        {
            let mut table = txn.open_table(KV)?;
            table.insert(key, value)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn load_bpm(&self) -> HashMap<String, f32> {
        match self.try_load_bpm() {
            Ok(map) => map,
            Err(e) => {
                error!("store: load_bpm failed: {e}");
                HashMap::new()
            }
        }
    }

    fn try_load_bpm(&self) -> Result<HashMap<String, f32>, StoreError> {
        let Some(db) = self.db() else {
            return Ok(HashMap::new());
        };
        let txn = db.begin_read()?;
        let table = match txn.open_table(BPM) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(HashMap::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = HashMap::new();
        for row in table.iter()? {
            let (id, bpm) = row?;
            out.insert(id.value().to_string(), bpm.value() as f32);
        }
        Ok(out)
    }

    pub fn set_bpm(&self, id: &str, bpm: f32) {
        if let Err(e) = self.try_set_bpm(id, bpm) {
            error!("store: set_bpm failed: {e}");
        }
    }

    fn try_set_bpm(&self, id: &str, bpm: f32) -> Result<(), StoreError> {
        let Some(db) = self.db() else { return Ok(()) };
        let txn = db.begin_write()?;
        {
            let mut table = txn.open_table(BPM)?;
            table.insert(id, bpm as f64)?;
        }
        txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> Track {
        Track {
            id: Some(id.to_string()),
            uri: format!("spotify:track:{id}"),
            title: id.to_string(),
            track_number: 0,
            disc_number: 0,
            duration: 0,
            artists: vec![],
            artist_ids: vec![],
            album: None,
            album_id: None,
            album_artists: vec![],
            cover_url: None,
            url: String::new(),
            added_at: None,
            list_index: 0,
            is_local: false,
            is_playable: Some(true),
        }
    }

    #[test]
    fn saved_tracks_round_trip_and_truncate() {
        let store = Store::in_memory();
        let tracks: Vec<Track> = ["a", "b", "c", "d"].iter().map(|id| track(id)).collect();
        store.replace_saved_range(0, &tracks);
        store.set_saved_total(4);

        assert_eq!(
            store
                .load_saved_tracks()
                .iter()
                .filter_map(|t| t.id.clone())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"]
        );
        assert_eq!(store.saved_total(), Some(4));

        // Replace just the middle two, then drop the tail.
        store.replace_saved_range(1, &[track("x"), track("y")]);
        store.truncate_saved(3);
        assert_eq!(
            store
                .load_saved_tracks()
                .iter()
                .filter_map(|t| t.id.clone())
                .collect::<Vec<_>>(),
            vec!["a", "x", "y"]
        );
    }

    #[test]
    fn bpm_round_trip() {
        let store = Store::in_memory();
        store.set_bpm("a", 128.0);
        store.set_bpm("b", 90.5);
        store.set_bpm("a", 129.0);
        let map = store.load_bpm();
        assert_eq!(map.get("a"), Some(&129.0));
        assert_eq!(map.get("b"), Some(&90.5));
    }
}
