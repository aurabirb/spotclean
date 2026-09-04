//! Local BPM detection and its scheduling.
//!
//! One background worker owns a cursor over the Liked Songs list and walks it forward, detecting
//! the BPM of each track that doesn't have one - no faster than
//! [`bpm_scan_min_interval_secs`](crate::config::ConfigValues). Resting the selection on a row
//! moves the cursor there (after [`bpm_scan_delay_secs`](crate::config::ConfigValues)), so
//! browsing prioritises what's on screen. The track you're *playing* jumps the cursor entirely and
//! is analyzed off the rate limit - but from librespot's audio cache once playback has filled it,
//! not a competing CDN fetch that the CDN can rate-limit into a stall. Until that cache file shows
//! up the playing track is just re-checked every few seconds (for up to [`PLAY_CACHE_GRACE`]).
//!
//! With [`bpm_scan_full_library`](crate::config::ConfigValues) on (the default) the cursor also
//! pages more of the list in as it reaches the end and wraps around, so left running it fills in
//! the whole library. `bpm::detect_bpm` reads from librespot's audio cache when the file is there
//! and otherwise streams just the ~1 minute it needs.
//!
//! Scanning can be turned off at runtime (see [`BpmScanner::set_enabled`]) for when the CDN
//! rate limit is better spent entirely on playback.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::bpm::{BpmOutcome, ScanOptions, detect_bpm};
use crate::config::Config;
use crate::events::EventManager;
use crate::liked_songs::LikedSongs;
use crate::model::track::Track;
use crate::spotify::Spotify;
use crate::store::Store;

/// Default seconds the selection must rest on a row before the cursor follows it there.
const DEFAULT_BPM_SCAN_DELAY_SECS: u64 = 1;
/// Default minimum wall-clock gap between two BPM audio fetches.
const DEFAULT_BPM_SCAN_MIN_INTERVAL_SECS: u64 = 15;
/// How often the worker wakes.
const TICK: Duration = Duration::from_millis(400);
/// After this many failed detections for one track, stop trying it this session.
const MAX_DETECT_ATTEMPTS: u8 = 3;
/// Back off this long when there's nothing to analyze within reach.
const IDLE: Duration = Duration::from_secs(30);
/// Back off this long once a fully-loaded library has been fully analyzed.
const IDLE_COMPLETE: Duration = Duration::from_secs(600);
/// Back off this long after requesting another page of the list.
const PAGE_LOAD_WAIT: Duration = Duration::from_secs(5);
/// Rows to request past the loaded end when the cursor catches up to it.
const PAGE: usize = 200;
/// How long to keep waiting for playback to populate the audio cache for the playing track before
/// giving up on it and fetching it directly, like any other track.
const PLAY_CACHE_GRACE: Duration = Duration::from_secs(150);
/// Gap between checks for the playing track's cache file while still within the grace period.
const PLAY_CACHE_POLL: Duration = Duration::from_secs(8);

struct Inner {
    spotify: Spotify,
    store: Store,
    cfg: Arc<Config>,
    ev: EventManager,
    liked: LikedSongs,
    /// BPM by track id. The single source of truth for tempo: views resolve it here (see
    /// [`Track::current_bpm`]) rather than carrying a BPM on their own possibly-stale clones.
    bpm_cache: RwLock<HashMap<String, f32>>,
    /// Failed-detection count per track id. At [`MAX_DETECT_ATTEMPTS`] the track is given up on
    /// for the session (a transient failure still retries).
    failures: RwLock<HashMap<String, u8>>,
    /// The track the user is playing - analyzed ahead of everything else and off the rate limit,
    /// from the audio cache playback is filling. Held until detection settles or the grace period
    /// for that cache to appear runs out. Also covers playing a track that isn't in Liked Songs.
    playing: Mutex<Option<PlayingTrack>>,
    /// The row the selection is resting on (id + when it landed there); the cursor follows it
    /// once it's been rested on for the dwell delay.
    focus: Mutex<Option<(String, Instant)>>,
    /// Whether the background walk runs at all. The playing track is still analyzed when this is
    /// off - it's already being downloaded for playback, so it costs no extra CDN traffic.
    enabled: AtomicBool,
}

/// The playing track while the scanner waits for playback to cache its audio.
struct PlayingTrack {
    track: Track,
    /// When playback of this track started.
    since: Instant,
    /// Earliest time to make the next detection attempt, so a cache miss doesn't spin.
    retry_after: Instant,
}

/// Owns the BPM cache and the detection worker. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct BpmScanner {
    inner: Arc<Inner>,
}

impl BpmScanner {
    fn build(
        spotify: Spotify,
        store: Store,
        cfg: Arc<Config>,
        ev: EventManager,
        liked: LikedSongs,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                spotify,
                store,
                cfg,
                ev,
                liked,
                bpm_cache: RwLock::new(HashMap::new()),
                failures: RwLock::new(HashMap::new()),
                playing: Mutex::new(None),
                focus: Mutex::new(None),
                enabled: AtomicBool::new(true),
            }),
        }
    }

    /// Create a scanner with no background worker, for use in tests.
    #[cfg(test)]
    pub fn disconnected(
        spotify: Spotify,
        store: Store,
        cfg: Arc<Config>,
        ev: EventManager,
        liked: LikedSongs,
    ) -> Self {
        Self::build(spotify, store, cfg, ev, liked)
    }

    /// Create a scanner, seed the BPM cache from the store and spawn the background worker.
    pub fn new(
        spotify: Spotify,
        store: Store,
        cfg: Arc<Config>,
        ev: EventManager,
        liked: LikedSongs,
    ) -> Self {
        let scanner = Self::build(spotify, store, cfg, ev, liked);
        *scanner.inner.bpm_cache.write().unwrap() = scanner.inner.store.load_bpm();
        let enabled = scanner
            .inner
            .cfg
            .values()
            .bpm_scan_enabled
            .unwrap_or(true);
        scanner.inner.enabled.store(enabled, Ordering::Relaxed);
        scanner.spawn_worker();
        scanner
    }

    /// Whether the background walk is currently running.
    pub fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Relaxed)
    }

    /// Flip the background walk on/off. The playing track keeps being analyzed regardless.
    /// Returns the new state.
    pub fn toggle_enabled(&self) -> bool {
        let enabled = !self.is_enabled();
        self.inner.enabled.store(enabled, Ordering::Relaxed);
        self.inner.ev.trigger();
        enabled
    }

    /// The freshest known BPM for `track_id`, if any.
    pub fn bpm_for(&self, track_id: &str) -> Option<f32> {
        self.inner.bpm_cache.read().unwrap().get(track_id).copied()
    }

    /// Whether a further lookup for `track_id` is pointless: we have its BPM, or detection has
    /// failed for it too many times this session.
    fn settled(&self, track_id: &str) -> bool {
        self.bpm_for(track_id).is_some()
            || self
                .inner
                .failures
                .read()
                .unwrap()
                .get(track_id)
                .is_some_and(|&n| n >= MAX_DETECT_ATTEMPTS)
    }

    /// Whether it's worth analyzing `track`: metadata enrichment is on, it's a remote track, and
    /// it isn't already settled. Cheap - safe to call every redraw / while scanning the list.
    pub fn wants_bpm_scan(&self, track: &Track) -> bool {
        if !self.inner.cfg.values().enrich_metadata.unwrap_or(true) || track.is_local {
            return false;
        }
        track.id.as_deref().is_some_and(|id| !self.settled(id))
    }

    /// Register `track` as the row the selection is on. The cursor follows it once it's been the
    /// focus for [`bpm_scan_delay_secs`](crate::config::ConfigValues), so scrolling *past* a row
    /// never moves the cursor.
    pub fn note_track_selection(&self, track: &Track) {
        let Some(id) = track.id.as_deref() else {
            return;
        };
        let mut focus = self.inner.focus.lock().unwrap();
        if focus.as_ref().map(|(fid, _)| fid.as_str()) == Some(id) {
            return; // still the same row - keep the dwell timer running
        }
        *focus = Some((id.to_string(), Instant::now()));
    }

    /// Note that `track` has just started playing. It's analyzed ahead of everything else and off
    /// the rate limit, but from the audio cache playback is filling rather than a competing CDN
    /// fetch - so the worker just re-checks it until that cache file appears (or the grace period
    /// lapses).
    pub fn note_track_played(&self, track: &Track) {
        if self.wants_bpm_scan(track) {
            let now = Instant::now();
            *self.inner.playing.lock().unwrap() = Some(PlayingTrack {
                track: track.clone(),
                since: now,
                retry_after: now,
            });
        }
    }

    /// The focused row's id, if it's been rested on for at least the dwell delay. Consumed.
    fn take_dwelled_focus(&self) -> Option<String> {
        let delay = Duration::from_secs(
            self.cfg_secs(|v| v.bpm_scan_delay_secs, DEFAULT_BPM_SCAN_DELAY_SECS),
        );
        let mut focus = self.inner.focus.lock().unwrap();
        match focus.as_ref() {
            Some((_, since)) if since.elapsed() >= delay => focus.take().map(|(id, _)| id),
            _ => None,
        }
    }

    fn cfg_secs(
        &self,
        pick: impl Fn(&crate::config::ConfigValues) -> Option<u64>,
        default: u64,
    ) -> u64 {
        let values = self.inner.cfg.values();
        pick(&values).unwrap_or(default)
    }

    fn spawn_worker(&self) {
        let scanner = self.clone();
        thread::Builder::new()
            .name("bpm-scanner".into())
            .spawn(move || scanner.run())
            .expect("failed to spawn bpm-scanner thread");
    }

    fn run(&self) {
        let saved = self.inner.liked.saved_tracks();
        let mut cursor = 0usize;
        let mut last_fetch: Option<Instant> = None;
        // While set and in the future, skip the (whole-list) scan but keep polling.
        let mut backoff_until: Option<Instant> = None;
        // Consecutive whole-list scans of a complete library that found nothing.
        let mut idle_passes = 0u32;

        loop {
            thread::sleep(TICK);

            // The playing track goes first, off the rate limit: analyzed from the audio cache
            // that playback is filling. An attempt short-circuits the tick; between attempts
            // (waiting on the cache) the ordinary walk carries on.
            if self.handle_playing() {
                continue;
            }

            // With the background walk turned off, the playing track above is all we do.
            if !self.is_enabled() {
                continue;
            }

            // Follow the selection every tick (cheap), even while rate-limited.
            if let Some(id) = self.take_dwelled_focus()
                && let Some(pos) = saved
                    .read()
                    .unwrap()
                    .iter()
                    .position(|t| t.id.as_deref() == Some(id.as_str()))
            {
                cursor = pos;
                backoff_until = None;
                idle_passes = 0;
            }

            let interval = Duration::from_secs(self.cfg_secs(
                |v| v.bpm_scan_min_interval_secs,
                DEFAULT_BPM_SCAN_MIN_INTERVAL_SECS,
            ));
            if last_fetch.is_some_and(|last| last.elapsed() < interval) {
                continue;
            }
            if backoff_until.is_some_and(|t| Instant::now() < t) {
                continue;
            }
            backoff_until = None;

            let (next, loaded) = {
                let tracks = saved.read().unwrap();
                let next = (cursor..tracks.len())
                    .find(|&i| self.wants_bpm_scan(&tracks[i]))
                    .map(|i| (i, tracks[i].clone()));
                (next, tracks.len())
            };

            if let Some((idx, track)) = next {
                self.analyze(&track, self.cdn_scan_options());
                cursor = idx + 1;
                idle_passes = 0;
                last_fetch = Some(Instant::now());
                continue;
            }

            // Nothing to analyze from the cursor to the end of the loaded list.
            if !self.inner.cfg.values().bpm_scan_full_library.unwrap_or(true) {
                backoff_until = Some(Instant::now() + IDLE);
            } else if self.inner.liked.total().is_some_and(|total| loaded >= total) {
                cursor = 0;
                idle_passes += 1;
                if idle_passes >= 2 {
                    backoff_until = Some(Instant::now() + IDLE_COMPLETE);
                }
            } else {
                self.inner.liked.ensure_through(loaded + PAGE);
                backoff_until = Some(Instant::now() + PAGE_LOAD_WAIT);
            }
        }
    }

    /// Make progress on the playing track, if there is one. Returns `true` if this consumed the
    /// tick - either a detection attempt ran, or the track is settled/gone - so the caller should
    /// `continue`. Returns `false` when it's just between cache re-checks, leaving the rest of the
    /// tick free for the ordinary walk.
    fn handle_playing(&self) -> bool {
        let (track, grace_elapsed) = {
            let mut guard = self.inner.playing.lock().unwrap();
            let Some(pending) = guard.as_ref() else {
                return false;
            };
            let Some(id) = pending.track.id.clone() else {
                *guard = None;
                return false;
            };
            if self.settled(&id) {
                *guard = None;
                return false;
            }
            if Instant::now() < pending.retry_after {
                return false; // cooling down between cache checks
            }
            (
                pending.track.clone(),
                pending.since.elapsed() >= PLAY_CACHE_GRACE,
            )
        };

        // While playback is still filling the cache, insist on reading from it; only once the
        // grace period has lapsed (playback long since moved on or stopped) fall back to a direct
        // fetch, which by then isn't competing with playback for the CDN.
        let outcome = self.analyze(
            &track,
            ScanOptions {
                require_cached: !grace_elapsed,
                ..self.cdn_scan_options()
            },
        );

        let mut guard = self.inner.playing.lock().unwrap();
        // A newer track may have replaced this one while detection was running.
        if guard.as_ref().and_then(|p| p.track.id.clone()) != track.id {
            return true;
        }
        match outcome {
            BpmOutcome::Detected(_) | BpmOutcome::Indeterminate => *guard = None,
            BpmOutcome::Unavailable if grace_elapsed => *guard = None,
            BpmOutcome::Unavailable => {
                if let Some(pending) = guard.as_mut() {
                    pending.retry_after = Instant::now() + PLAY_CACHE_POLL;
                }
            }
        }
        true
    }

    /// CDN-fetch options shared by both scan paths: when `bpm_scan_cache_full` is set, request the
    /// player's configured bitrate and download the whole file so librespot caches it for
    /// playback. `require_cached` is filled in per call site.
    fn cdn_scan_options(&self) -> ScanOptions {
        let values = self.inner.cfg.values();
        if values.bpm_scan_cache_full.unwrap_or(true) {
            ScanOptions {
                require_cached: false,
                bitrate: Some(values.bitrate.unwrap_or(320)),
                cache_full: true,
            }
        } else {
            ScanOptions::default()
        }
    }

    /// Detect `track`'s BPM and store it, returning the outcome. No-op if it's already settled.
    ///
    /// With `opts.require_cached` set, only the audio cache is consulted - a miss is
    /// [`BpmOutcome::Unavailable`] and doesn't count against [`MAX_DETECT_ATTEMPTS`], since the
    /// audio may simply not have been downloaded yet. A failed *fetch* ([`BpmOutcome::Unavailable`]
    /// without `require_cached`) and a ran-but-empty analysis ([`BpmOutcome::Indeterminate`]) both
    /// count. A missing session never counts.
    fn analyze(&self, track: &Track, opts: ScanOptions) -> BpmOutcome {
        let Some(track_id) = track.id.clone() else {
            return BpmOutcome::Unavailable;
        };
        if let Some(bpm) = self.bpm_for(&track_id) {
            return BpmOutcome::Detected(bpm);
        }
        if self.settled(&track_id) {
            return BpmOutcome::Indeterminate;
        }
        let Some(session) = self.inner.spotify.session() else {
            return BpmOutcome::Unavailable;
        };

        let require_cached = opts.require_cached;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            detect_bpm(&session, track, opts)
        }))
        .unwrap_or(BpmOutcome::Unavailable);

        match &outcome {
            BpmOutcome::Detected(bpm) => {
                self.inner
                    .bpm_cache
                    .write()
                    .unwrap()
                    .insert(track_id.clone(), *bpm);
                self.inner.failures.write().unwrap().remove(&track_id);
                self.inner.store.set_bpm(&track_id, *bpm);
                self.inner.ev.trigger();
            }
            BpmOutcome::Unavailable if require_cached => {
                log::debug!("bpm: {track_id} not in the audio cache yet");
            }
            BpmOutcome::Indeterminate | BpmOutcome::Unavailable => {
                let mut failures = self.inner.failures.write().unwrap();
                let n = failures.entry(track_id.clone()).or_insert(0);
                *n += 1;
                log::debug!(
                    "bpm: detection failed for {track_id} (attempt {n}/{MAX_DETECT_ATTEMPTS})"
                );
            }
        }
        outcome
    }
}
