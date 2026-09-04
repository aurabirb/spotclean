//! ncspot-fork-only `track_format` token expansion, kept out of the otherwise lightly-modified
//! upstream `model::playable` module to keep the merge diff small.

use crate::library::{Library, SortStatus};
use crate::model::playable::Playable;

/// Expand ncspot-fork-only track_format tokens (`%saved`, `%bpm`, `%sorted`) in an already
/// otherwise-expanded format string.
///
/// - `%saved`: Saved ("Liked Songs") state is no longer tracked locally; the token is kept so
///   existing `track_format` configs don't break, but it always renders empty now.
/// - `%bpm`: the track's detected BPM, formatted `{:.0}`, or empty.
/// - `%sorted`: the keys of the keybind-bound playlists this track is filed into, comma-joined,
///   or empty.
pub fn expand_fork_tokens(s: String, playable: &Playable, library: &Library) -> String {
    s.replace("%saved", "")
        .replace(
            "%bpm",
            &match playable.clone() {
                Playable::Track(track) => {
                    track.current_bpm(library).map(|bpm| format!("{bpm:.0}"))
                }
                _ => None,
            }
            .unwrap_or_default(),
        )
        .replace(
            "%sorted",
            &match playable.clone() {
                Playable::Track(track) => track.id.as_deref().map(|id| library.sort_status(id)),
                _ => None,
            }
            .map_or(String::new(), |status| match status {
                SortStatus::Sorted(keys) => keys.join(","),
                SortStatus::Unsorted | SortStatus::Unavailable => String::new(),
            }),
        )
}
