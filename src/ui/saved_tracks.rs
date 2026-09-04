use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use cursive::Cursive;
use cursive::Vec2;
use cursive::view::{View, ViewWrapper};

use crate::command::Command;
use crate::commands::CommandResult;
use crate::library::Library;
use crate::model::playlist::Playlist;
use crate::model::track::Track;
use crate::queue::Queue;
use crate::spotify::PlayerEvent;
use crate::traits::ViewExt;
use crate::ui::listview::ListView;

/// Keep the filtered list filled at least this far past the last visible row (roughly two
/// screens), so scrolling stays ahead of the page loader.
const LOOKAHEAD: usize = 100;

/// The "Tracks" library tab: the user's Liked Songs. Backed by [`Library::saved_tracks`], which is
/// seeded from disk on launch and revalidated page-by-page as this list is scrolled.
///
/// This tab also drives sorting Liked Songs into playlists with one key per playlist (see
/// [`Command::AddToPlaylist`] and [`Command::BindKey`]). While `hide_sorted` is on (toggled with
/// [`Command::ToggleSortFilter`]), tracks already filed into a keybind-bound playlist are hidden.
/// The view keeps a filtered snapshot of [`Library::saved_tracks`] and rebuilds it whenever the
/// shared list changes.
pub struct SavedTracksView {
    list: ListView<Track>,
    /// Filtered rows shown by `list`.
    content: Arc<RwLock<Vec<Track>>>,
    library: Arc<Library>,
    queue: Arc<Queue>,
    hide_sorted: bool,
    /// `Library::saved_tracks_version` the filtered snapshot was last built against.
    seen_version: u64,
}

impl SavedTracksView {
    pub fn new(queue: Arc<Queue>, library: Arc<Library>) -> Self {
        let content = Arc::new(RwLock::new(Vec::new()));
        let list = ListView::new(content.clone(), queue.clone(), library.clone())
            .with_match_playing_by_id(true);

        let mut view = Self {
            list,
            content,
            library,
            queue,
            hide_sorted: false,
            seen_version: u64::MAX,
        };
        view.library.ensure_saved_tracks_through(0);
        view.rebuild();
        view.keep_ahead();
        view
    }

    /// Ids of liked tracks already filed into a keybind-bound playlist, hidden while
    /// `hide_sorted` is on. Computed from `library.playlists`, which is loaded in full.
    fn hidden_ids(&self) -> HashSet<String> {
        if !self.hide_sorted {
            return HashSet::new();
        }
        let bound_names = self.library.bound_playlist_names();
        self.library
            .playlists
            .read()
            .unwrap()
            .iter()
            .filter(|p| bound_names.contains(&p.name.to_ascii_lowercase()))
            .flat_map(|p| p.tracks.iter().flatten())
            .filter_map(|t| t.id())
            .collect()
    }

    /// Recompute the filtered snapshot from `library.saved_tracks`. With `hide_sorted` off this
    /// copies the whole list through unchanged, so the tab behaves like a plain Liked Songs list.
    fn rebuild(&mut self) {
        let hidden = self.hidden_ids();
        let filtered: Vec<Track> = self
            .library
            .saved_tracks
            .read()
            .unwrap()
            .iter()
            .filter(|t| t.id.as_deref().is_none_or(|id| !hidden.contains(id)))
            .cloned()
            .collect();
        *self.content.write().unwrap() = filtered;
        self.seen_version = self.library.saved_tracks_version();
    }

    /// Rebuild if the shared list changed, then ask for more raw rows if the filtered list is
    /// running short of the scroll position.
    fn sync(&mut self) {
        if self.library.saved_tracks_version() != self.seen_version {
            self.rebuild();
        }
        self.keep_ahead();
    }

    /// Tell the library which row the selection is on, so the BPM scheduler's cursor can follow
    /// it once it's been rested on. Cheap to call every layout.
    fn notify_selected(&self) {
        let content = self.content.read().unwrap();
        if let Some(track) = content.get(self.list.selected_index()) {
            self.library.note_track_selection(track);
        }
    }

    /// Keybind-bound playlists that currently contain `track_id`. Used by `RemoveFromPlaylists`
    /// to drop a track from all of them at once.
    fn bound_playlists_containing(&self, track_id: &str) -> Vec<Playlist> {
        let bound_names = self.library.bound_playlist_names();
        self.library
            .playlists
            .read()
            .unwrap()
            .iter()
            .filter(|p| bound_names.contains(&p.name.to_ascii_lowercase()) && p.has_track(track_id))
            .cloned()
            .collect()
    }

    fn keep_ahead(&self) {
        let want = self.list.last_visible_index() + LOOKAHEAD;
        let content_len = self.content.read().unwrap().len();
        if content_len >= want {
            return;
        }
        // We need `want` filtered rows but only have `content_len`. Filtered rows are a subset of
        // the raw list, so translate the target into raw-list terms by the current keep rate
        // (raw_len / content_len) and ask the library to validate/fetch that far - matching the
        // scroll position rather than just nudging one page past the end, so fast scrolling
        // doesn't outrun the loader.
        let raw_len = self.library.saved_tracks.read().unwrap().len();
        let target_raw = want
            .saturating_mul(raw_len)
            .checked_div(content_len)
            .unwrap_or(want);
        self.library
            .ensure_saved_tracks_through(target_raw + LOOKAHEAD);
    }
}

impl ViewWrapper for SavedTracksView {
    wrap_impl!(self.list: ListView<Track>);

    fn wrap_layout(&mut self, size: Vec2) {
        // Size the scrollbar against the full liked-songs count while the list is still paging in
        // (only when not filtering, where the visible count is genuinely smaller).
        let virtual_len = if self.hide_sorted {
            0
        } else {
            self.library.saved_tracks_total().unwrap_or(0)
        };
        self.list.set_virtual_len(virtual_len);
        self.list.layout(size);
        self.sync();
        self.notify_selected();
    }

    /// Re-lay-out (and therefore rebuild the filtered snapshot) whenever the shared Liked Songs
    /// list has changed - e.g. a background page fetch appended rows. Without this the newly
    /// loaded rows wouldn't show until some unrelated relayout happened to occur.
    fn wrap_needs_relayout(&self) -> bool {
        self.library.saved_tracks_version() != self.seen_version || self.list.needs_relayout()
    }
}

impl ViewExt for SavedTracksView {
    fn title(&self) -> String {
        "Tracks".to_string()
    }

    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        // Previous normally restarts the current track instead of actually going back when
        // more than 5s in (see Command::Previous in commands.rs), so a single press here would
        // usually just restart whatever's playing rather than move to the previous row - not
        // useful while sorting, where "go back a track" always means the adjacent row.
        if let Command::Previous = cmd {
            self.queue.previous();
            return Ok(CommandResult::Consumed(None));
        }

        if let Command::ToggleSortFilter = cmd {
            self.hide_sorted = !self.hide_sorted;
            self.rebuild();
            self.list.move_focus_to(0);
            self.keep_ahead();
            return Ok(CommandResult::Consumed(Some(format!(
                "Hiding already-sorted tracks: {}",
                if self.hide_sorted { "on" } else { "off" }
            ))));
        }

        // Filing a track away makes it disappear from this list (when hide_sorted is on).
        // Removing just that one row is far cheaper than recomputing the whole filtered list.
        // Filing / unfiling a track is a Liked-Songs workflow, so the real playlist mutation
        // lives here rather than in the generic inner `ListView`.
        if let Command::AddToPlaylist(name) = cmd {
            let selected = self.list.selected_index();
            let track = self.content.read().unwrap().get(selected).cloned();
            let Some(track) = track else {
                return Ok(CommandResult::Consumed(None));
            };
            let sorted_id = track.id.clone();

            let playlist = self
                .library
                .playlists
                .read()
                .unwrap()
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(name))
                .cloned();
            let Some(playlist) = playlist else {
                return Err(format!("No playlist named \"{name}\""));
            };

            // In hidden-sort mode the just-sorted track is about to disappear from view; if it's
            // also the track currently playing, follow it by advancing playback rather than
            // leaving the now-invisible track playing on its own. Only when it's actually playing.
            if self.hide_sorted
                && sorted_id.is_some()
                && self.queue.get_current().and_then(|t| t.id()) == sorted_id
                && matches!(
                    self.queue.get_spotify().get_current_status(),
                    PlayerEvent::Playing(_)
                )
            {
                self.queue.next(false);
            }

            // A plain toggle for the one playlist whose key was pressed: if the track is already
            // in it, remove it; otherwise add it. The actual Spotify API calls happen on the
            // library's background playlist worker, so filing never freezes the app.
            let already_filed = track.id.as_deref().is_some_and(|id| playlist.has_track(id));
            if already_filed {
                self.library
                    .queue_playlist_mutation(track, vec![playlist], None);
            } else {
                self.library
                    .queue_playlist_mutation(track, vec![], Some(playlist));
            }

            if self.hide_sorted
                && let Some(id) = sorted_id
            {
                // Removing the just-sorted row shifts everything after it down into its spot, so
                // the selection (left untouched) already lands on the next track - except when
                // the removed row was the last one, so re-clamp the selection afterwards.
                self.content
                    .write()
                    .unwrap()
                    .retain(|t| t.id.as_deref() != Some(id.as_str()));
                self.list.move_focus_to(self.list.selected_index());
                self.keep_ahead();
            }

            return Ok(CommandResult::Consumed(None));
        }

        if let Command::RemoveFromPlaylists = cmd {
            let selected = self.list.selected_index();
            let track = self.content.read().unwrap().get(selected).cloned();
            let Some(track) = track else {
                return Ok(CommandResult::Consumed(None));
            };

            // Drop the track from every keybind-bound playlist it's currently in. Liked Songs is
            // never touched.
            let bound_playlists = track
                .id
                .as_deref()
                .map_or_else(Vec::new, |id| self.bound_playlists_containing(id));
            self.library
                .queue_playlist_mutation(track, bound_playlists, None);
            return Ok(CommandResult::Consumed(None));
        }

        let result = self.list.on_command(s, cmd);
        self.sync();
        // Keyboard navigation moves the selection without a relayout, so register the new row
        // (and its look-ahead) for a BPM scan here too, not only on layout.
        self.notify_selected();
        result
    }
}
