use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::library::Library;
use crate::model::playable::Playable;
use crate::spotify::Spotify;

/// How long [`LoadDebouncer::dispatch`] waits, after the most recent call, before actually
/// loading the track.
const LOAD_DEBOUNCE: Duration = Duration::from_millis(150);

/// Debounces track loads triggered by rapid Previous/Next presses.
///
/// Without this, rapidly repeating Previous/Next (e.g. holding the key, which triggers
/// terminal key-repeat) sends a burst of loads to the player faster than a single audio key
/// exchange with Spotify completes. Each new load cancels whichever fetch was still in
/// flight for the previous one, which can leave playback stuck retrying for several seconds
/// after the user has already stopped and settled on a track. Debouncing means only the
/// track the user actually lands on gets an audio key request sent for it.
pub struct LoadDebouncer {
    /// Bumped on every [`LoadDebouncer::dispatch`] call; used to detect whether a later call
    /// has superseded an earlier, still-debouncing one.
    generation: Arc<AtomicU64>,
    spotify: Spotify,
    #[cfg_attr(not(feature = "notify"), allow(dead_code))]
    cfg: Arc<Config>,
    library: Arc<Library>,
}

impl LoadDebouncer {
    pub fn new(spotify: Spotify, cfg: Arc<Config>, library: Arc<Library>) -> Self {
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            spotify,
            cfg,
            library,
        }
    }

    /// Load `track` after `LOAD_DEBOUNCE` ms unless a newer `dispatch()` supersedes it.
    pub fn dispatch(&self, track: Playable) {
        let generation = self.generation.fetch_add(1, AtomicOrdering::SeqCst) + 1;

        let spotify = self.spotify.clone();
        let library = self.library.clone();
        #[cfg(feature = "notify")]
        let cfg = self.cfg.clone();
        let load_generation = self.generation.clone();

        thread::spawn(move || {
            thread::sleep(LOAD_DEBOUNCE);
            if load_generation.load(AtomicOrdering::SeqCst) != generation {
                // A newer dispatch() call came in before the debounce elapsed; let it win.
                return;
            }

            spotify.load(&track, true, 0);
            spotify.update_track();
            // The track's audio is now being cached by playback; let the BPM scanner analyze it
            // from that cache rather than a separate CDN fetch.
            library.note_track_played(&track);

            #[cfg(feature = "notify")]
            if cfg.values().notify.unwrap_or(false) {
                // use same parser as track_format, Playable::format
                let format = cfg.values().notification_format.clone().unwrap_or_default();
                let default_title = crate::config::NotificationFormat::default().title.unwrap();
                let title = format.title.unwrap_or_else(|| default_title.clone());

                let default_body = crate::config::NotificationFormat::default().body.unwrap();
                let body = format.body.unwrap_or_else(|| default_body.clone());

                let summary_txt = Playable::format(&track, &title, &library);
                let body_txt = Playable::format(&track, &body, &library);
                let cover_url = track.cover_url();
                crate::queue::send_notification(&summary_txt, &body_txt, cover_url);
            }

            // Send a Seeked signal at start of new track
            #[cfg(feature = "mpris")]
            spotify.notify_seeked(0);
        });
    }
}
