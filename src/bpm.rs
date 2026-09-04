//! Offline BPM detection straight from a track's Spotify audio.
//!
//! Instead of asking a third-party metadata API for a track's tempo, this fetches, decrypts and
//! decodes the same Ogg Vorbis stream the player would use (transparently served from librespot's
//! on-disk audio cache when the track has been played before) and estimates the tempo from the
//! first minute or so of it. Nothing here touches or depends on playback - the fetch/decrypt/decode
//! path mirrors what `librespot_playback::player::PlayerTrackLoader` runs internally, trimmed down
//! to what a tempo estimate needs.
//!
//! The estimator is a clean-room implementation of standard beat-tracking DSP (no code taken from
//! any GPL library):
//!
//! 1. **Multi-band log-magnitude spectral flux** for the onset envelope, with a SuperFlux-style
//!    frequency max-filter on the reference frame to suppress vibrato/glissando false positives.
//! 2. **Per-band normalisation** before the bands are summed, so a loud kick drum doesn't drown
//!    out everything happening further up the spectrum.
//! 3. **Windowed autocorrelation with a harmonic comb filter**: each candidate tempo is scored by
//!    the autocorrelation energy at its period *and its integer multiples*, which makes the true
//!    tempo win over its own half/double time.
//! 4. **Aggregation across overlapping windows** (mode of the per-window estimates), so an intro,
//!    breakdown or slight drift doesn't throw the whole track off.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;

use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::{Session, SpotifyId, SpotifyUri};
use librespot_metadata::audio::{AudioFileFormat, AudioItem};
use librespot_playback::decoder::{AudioDecoder, AudioPacket, SymphoniaDecoder};
use rustfft::{Fft, FftPlanner, num_complex::Complex};
use symphonia::core::io::MediaSource;
use symphonia::core::probe::Hint;

use crate::application::ASYNC_RUNTIME;
use crate::model::track::Track;

/// Spotify prepends a proprietary header to its Ogg Vorbis streams; real Vorbis data starts here.
const SPOTIFY_OGG_HEADER_END: u64 = 0xa7;

/// librespot's fixed output sample rate (it resamples nothing; every Spotify stream is 44.1 kHz).
const SAMPLE_RATE: f32 = 44_100.0;

/// How much audio, from the start of the track, to analyse. A minute is plenty for a stable tempo
/// estimate and keeps the CDN download small for tracks that aren't in the audio cache yet.
///
/// Analysing from the opening rather than a later offset is deliberate: on a spread of test tracks,
/// sampling from deeper in (a chorus, a breakdown, a half-time section) was measurably *worse* -
/// the first minute is usually the most metronomic, least arranged part of a song.
const ANALYSIS_SAMPLES: usize = (SAMPLE_RATE as usize) * 60;

/// STFT window and hop for the onset envelope.
const FFT_SIZE: usize = 2048;
const HOP: usize = 512;

/// Onset-envelope sample rate (one value per STFT hop).
const ONSET_RATE: f32 = SAMPLE_RATE / HOP as f32;

/// Number of log-spaced frequency bands the spectral flux is split into.
const BANDS: usize = 8;

/// Lowest / highest frequency (Hz) considered for onsets. Sub-bass is mostly rumble; the top
/// octave is mostly cymbal wash and hiss.
const LOW_HZ: f32 = 30.0;
const HIGH_HZ: f32 = 11_000.0;

/// Radius, in FFT bins, of the max-filter applied to the reference frame (SuperFlux).
const MAX_FILTER_RADIUS: usize = 3;

/// Tempo search range. Everything outside this is treated as a bad estimate.
const MIN_BPM: f32 = 60.0;
const MAX_BPM: f32 = 200.0;

/// Length / hop of the analysis windows the tempo is estimated on, in seconds.
const WIN_SECONDS: f32 = 8.0;
const WIN_HOP_SECONDS: f32 = 4.0;

/// How many harmonics of a candidate period the comb filter sums over.
const COMB_HARMONICS: usize = 4;

/// The result of a BPM detection attempt.
pub enum BpmOutcome {
    /// A confident estimate, rounded to the nearest whole BPM.
    Detected(f32),
    /// The audio was fetched and analyzed but no plausible tempo came out. Re-running the same
    /// analysis won't help - the caller should stop trying this track.
    Indeterminate,
    /// The audio couldn't be obtained or decoded: no session, nothing in the audio cache when
    /// `require_cached` was set, a CDN error, or a truncated stream. Transient - worth another
    /// try once the cache has filled or the rate limit has passed.
    Unavailable,
}

/// Detect `track`'s BPM from its audio. Blocking and moderately CPU-heavy (a Vorbis decode of
/// ~1 minute of audio plus a batch of FFTs) - call it from a background worker, never the UI
/// thread.
///
/// With `require_cached` set, the analysis only runs when one of the track's Ogg files is
/// already in librespot's on-disk audio cache; otherwise it returns [`BpmOutcome::Unavailable`]
/// rather than pulling the bytes from the CDN. Use it for the track that's currently playing:
/// playback is filling that cache anyway, and a competing CDN fetch can be rate-limited into a
/// stall.
pub fn detect_bpm(session: &Session, track: &Track, require_cached: bool) -> BpmOutcome {
    let Ok(uri) = SpotifyUri::from_uri(&track.uri) else {
        return BpmOutcome::Unavailable;
    };
    let Some(runtime) = ASYNC_RUNTIME.get() else {
        return BpmOutcome::Unavailable;
    };

    let Some((source, length)) =
        runtime.block_on(open_audio_stream(session, uri, require_cached))
    else {
        return BpmOutcome::Unavailable;
    };
    // `Subfile` seeks to the offset on construction, skipping Spotify's custom Ogg header so
    // symphonia sees a clean Vorbis stream.
    let Ok(subfile) = Subfile::new(source, SPOTIFY_OGG_HEADER_END, length) else {
        return BpmOutcome::Unavailable;
    };

    let Some(mono) = decode_mono(subfile) else {
        return BpmOutcome::Unavailable;
    };
    let onset = onset_envelope(&mono);
    match estimate_tempo(&onset) {
        Some(bpm) => BpmOutcome::Detected(bpm),
        None => BpmOutcome::Indeterminate,
    }
}

// --- Spotify audio retrieval -------------------------------------------------------------------

/// The Ogg Vorbis formats we're willing to analyze, smallest first.
const OGG_FORMATS: [AudioFileFormat; 3] = [
    AudioFileFormat::OGG_VORBIS_96,
    AudioFileFormat::OGG_VORBIS_160,
    AudioFileFormat::OGG_VORBIS_320,
];

/// Resolve `uri` to a playable Ogg Vorbis file and return its decrypting reader plus the file's
/// total length. Mirrors `PlayerTrackLoader::load_remote_track`, minus everything playback-specific.
async fn open_audio_stream(
    session: &Session,
    uri: SpotifyUri,
    require_cached: bool,
) -> Option<(AudioDecrypt<AudioFile>, u64)> {
    // The audio key is requested for the *original* track id even when playback falls back to an
    // alternative (region-relinked) file - same as librespot.
    let track_id: SpotifyId = (&uri).try_into().ok()?;

    let audio_item = AudioItem::get_file(session, uri).await.ok()?;
    let audio_item = find_available(session, audio_item).await?;

    // Prefer whichever Vorbis file is already in the local audio cache (i.e. the bitrate the
    // track was played at) so analysis reuses those bytes and never touches the CDN. Only when
    // nothing is cached do we fall back to the smallest available file, to keep that download
    // light.
    let available = || {
        OGG_FORMATS
            .iter()
            .filter_map(|f| audio_item.files.get(f).map(|id| (*f, *id)))
    };
    let cache = session.cache();
    let cached = available().find(|(_, id)| {
        cache
            .and_then(|c| c.file_path(*id))
            .is_some_and(|p| p.exists())
    });
    let (format, file_id) = match cached {
        Some(hit) => hit,
        // Nothing cached: for the playing track we wait for playback to fill the cache rather
        // than race it to the CDN; otherwise fall back to the smallest file to keep it light.
        None if require_cached => return None,
        None => available().next()?,
    };

    let bytes_per_second = stream_data_rate(format);
    let encrypted = AudioFile::open(session, file_id, bytes_per_second)
        .await
        .ok()?;
    let length = encrypted.get_stream_loader_controller().ok()?.len() as u64;

    // Some files aren't encrypted; if the key request fails, continue undecrypted and let the
    // decoder bail if it turns out the data really was scrambled (same policy as librespot).
    let key = session.audio_key().request(track_id, file_id).await.ok();

    Some((AudioDecrypt::new(key, encrypted), length))
}

/// A track is playable as-is if it has files and is available; otherwise follow its alternatives
/// (region-relinked equivalents) and take the first that is. Condensed from
/// `PlayerTrackLoader::find_available_alternative`.
async fn find_available(session: &Session, audio_item: AudioItem) -> Option<AudioItem> {
    if audio_item.availability.is_err() {
        return None;
    }
    if !audio_item.files.is_empty() {
        return Some(audio_item);
    }

    for alt_uri in audio_item.alternatives?.0 {
        if let Ok(alt) = AudioItem::get_file(session, alt_uri).await
            && alt.availability.is_ok()
            && !alt.files.is_empty()
        {
            return Some(alt);
        }
    }
    None
}

/// Nominal bytes per second for a format, used to size streaming reads. Values match librespot's
/// `PlayerTrackLoader::stream_data_rate` (kilobytes/s * 1024).
fn stream_data_rate(format: AudioFileFormat) -> usize {
    let kbps = match format {
        AudioFileFormat::OGG_VORBIS_96 => 12.0,
        AudioFileFormat::OGG_VORBIS_160 => 20.0,
        _ => 40.0,
    };
    (kbps * 1024.0_f32).ceil() as usize
}

/// Decode the stream to a mono f32 signal, stopping after [`ANALYSIS_SAMPLES`].
fn decode_mono<R: MediaSource + 'static>(source: R) -> Option<Vec<f32>> {
    let mut hint = Hint::new();
    hint.mime_type("audio/ogg");

    let mut decoder = SymphoniaDecoder::new(source, hint).ok()?;
    let mut mono = Vec::with_capacity(ANALYSIS_SAMPLES);

    while mono.len() < ANALYSIS_SAMPLES {
        let Ok(Some((_, packet))) = decoder.next_packet() else {
            break;
        };
        // Decoded packets are interleaved stereo f64.
        let AudioPacket::Samples(samples) = packet else {
            break;
        };
        let (frames, _) = samples.as_chunks::<2>();
        for frame in frames {
            mono.push(((frame[0] + frame[1]) * 0.5) as f32);
        }
    }

    (mono.len() >= FFT_SIZE * 8).then_some(mono)
}

// --- Onset envelope --------------------------------------------------------------------------

/// Multi-band log-magnitude spectral flux. For every STFT hop and every frequency band, the sum of
/// the positive changes in (log) magnitude relative to a frequency-max-filtered previous frame.
/// Each band's series is then normalised to unit variance before the bands are summed, so no single
/// part of the spectrum dominates the result.
fn onset_envelope(signal: &[f32]) -> Vec<f32> {
    let mut planner = FftPlanner::<f32>::new();
    let fft: Arc<dyn Fft<f32>> = planner.plan_fft_forward(FFT_SIZE);

    // Periodic Hann window.
    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|n| {
            (std::f32::consts::PI * n as f32 / FFT_SIZE as f32)
                .sin()
                .powi(2)
        })
        .collect();

    let bins = FFT_SIZE / 2 + 1;
    let band_edges = log_spaced_band_edges(bins);

    let frames = signal.len().saturating_sub(FFT_SIZE) / HOP + 1;
    let mut band_flux: [Vec<f32>; BANDS] = std::array::from_fn(|_| Vec::with_capacity(frames));

    let mut prev_log = vec![0.0_f32; bins];
    let mut cur_log = vec![0.0_f32; bins];
    let mut scratch = vec![Complex::<f32>::default(); FFT_SIZE];

    let mut pos = 0;
    let mut first = true;
    while pos + FFT_SIZE <= signal.len() {
        for (i, s) in scratch.iter_mut().enumerate() {
            *s = Complex::new(signal[pos + i] * window[i] / FFT_SIZE as f32, 0.0);
        }
        fft.process(&mut scratch);
        for (slot, bin) in cur_log.iter_mut().zip(&scratch) {
            *slot = (1.0 + 1000.0 * bin.norm()).ln();
        }

        if first {
            // No previous frame to diff against yet.
            for flux in band_flux.iter_mut() {
                flux.push(0.0);
            }
            first = false;
        } else {
            for (b, flux) in band_flux.iter_mut().enumerate() {
                let (lo, hi) = (band_edges[b], band_edges[b + 1]);
                let mut sum = 0.0_f32;
                for (k, &cur) in cur_log.iter().enumerate().take(hi).skip(lo) {
                    // SuperFlux: compare against the max of the reference frame over a small
                    // frequency neighbourhood, so vibrato (energy sliding between bins) isn't
                    // mistaken for an onset.
                    let ref_lo = k.saturating_sub(MAX_FILTER_RADIUS);
                    let ref_hi = (k + MAX_FILTER_RADIUS + 1).min(bins);
                    let reference = prev_log[ref_lo..ref_hi]
                        .iter()
                        .copied()
                        .fold(f32::MIN, f32::max);
                    let diff = cur - reference;
                    if diff > 0.0 {
                        sum += diff;
                    }
                }
                flux.push(sum);
            }
        }

        std::mem::swap(&mut prev_log, &mut cur_log);
        pos += HOP;
    }

    // Normalise each band to zero mean / unit variance, then sum.
    let len = band_flux.first().map_or(0, Vec::len);
    let mut envelope = vec![0.0_f32; len];
    for flux in &band_flux {
        let (mean, std) = mean_std(flux);
        if std <= f32::EPSILON {
            continue;
        }
        for (e, &f) in envelope.iter_mut().zip(flux) {
            *e += (f - mean) / std;
        }
    }

    // Remove a slow baseline (a gradual loudness swell) and half-wave rectify.
    detrend(&mut envelope, (ONSET_RATE * 0.5) as usize);
    envelope
}

/// FFT-bin indices splitting `[LOW_HZ, HIGH_HZ]` into [`BANDS`] logarithmically-spaced bands.
fn log_spaced_band_edges(bins: usize) -> Vec<usize> {
    let bin_of = |hz: f32| ((hz * FFT_SIZE as f32 / SAMPLE_RATE).round() as usize).clamp(1, bins);
    let (lo, hi) = (LOW_HZ.ln(), HIGH_HZ.ln());
    (0..=BANDS)
        .map(|b| {
            let frac = b as f32 / BANDS as f32;
            bin_of((lo + frac * (hi - lo)).exp())
        })
        .collect()
}

// --- Tempo estimation ------------------------------------------------------------------------

/// Centre (BPM) and log-space width of the perceptual tempo preference. Human "foot-tapping"
/// tempo clusters around here; used both to break ties between a period and its harmonics and to
/// pick the right octave at the end.
const TEMPO_CENTRE: f32 = 130.0;
const TEMPO_SIGMA: f32 = 0.55;

/// Perceptual plausibility weight for a tempo, peaking at [`TEMPO_CENTRE`].
fn tempo_weight(bpm: f32) -> f32 {
    (-0.5 * ((bpm / TEMPO_CENTRE).ln() / TEMPO_SIGMA).powi(2)).exp()
}

/// Estimate the tempo of an onset envelope. The envelope is cut into overlapping windows; each
/// window is autocorrelated and scored with a harmonic comb filter plus a perceptual tempo weight,
/// the per-window estimates are reduced to their mode, and a final octave check decides between
/// that value and its half / double time over the whole envelope.
fn estimate_tempo(onset: &[f32]) -> Option<f32> {
    let min_lag = (60.0 * ONSET_RATE / MAX_BPM).floor().max(1.0) as usize;
    let max_lag = (60.0 * ONSET_RATE / MIN_BPM).ceil() as usize;

    let win = (WIN_SECONDS * ONSET_RATE) as usize;
    let hop = (WIN_HOP_SECONDS * ONSET_RATE) as usize;
    if onset.len() < win.min(4 * max_lag) || max_lag <= min_lag {
        return None;
    }

    let mut estimates = Vec::new();
    let mut start = 0;
    while start + win <= onset.len() {
        if let Some(bpm) = window_tempo(&onset[start..start + win], min_lag, max_lag) {
            estimates.push(bpm);
        }
        start += hop;
    }
    // Very short envelope: fall back to a single pass over everything.
    if estimates.is_empty()
        && let Some(bpm) = window_tempo(onset, min_lag, max_lag)
    {
        estimates.push(bpm);
    }
    if estimates.is_empty() {
        return None;
    }

    let agg = aggregate(&estimates);
    bpm_debug(|| {
        let mut sorted = estimates.clone();
        sorted.sort_by(f32::total_cmp);
        format!(
            "windows={} agg={agg:.1} per-window={:?}",
            estimates.len(),
            sorted.iter().map(|b| b.round() as i32).collect::<Vec<_>>()
        )
    });

    let final_bpm = resolve_octave(onset, agg);
    (MIN_BPM..=MAX_BPM)
        .contains(&final_bpm)
        .then_some(final_bpm.round())
}

/// Decide between `bpm`, its half time and its double time by scoring each over the *whole* onset
/// envelope (better periodicity SNR than any single window) with the harmonic comb filter and the
/// perceptual tempo weight. Fixes the common failure where a backbeat-heavy track autocorrelates
/// most strongly at half its actual tempo.
fn resolve_octave(onset: &[f32], bpm: f32) -> f32 {
    let (mean, _) = mean_std(onset);
    let centred: Vec<f32> = onset.iter().map(|&x| x - mean).collect();
    let energy = centred.iter().map(|&x| x * x).sum::<f32>() / centred.len() as f32;
    if energy <= f32::EPSILON {
        return bpm;
    }

    let max_lag = centred.len() / 2;
    let comb = |candidate: f32| -> f32 {
        let base = 60.0 * ONSET_RATE / candidate;
        let mut acc = 0.0;
        let mut count = 0.0;
        for h in 1..=COMB_HARMONICS {
            let lag = base * h as f32;
            if lag as usize >= max_lag {
                break;
            }
            acc += autocorr_at(&centred, lag) / energy;
            count += 1.0;
        }
        if count == 0.0 { 0.0 } else { acc / count }
    };

    let scored: Vec<(f32, f32)> = [bpm * 0.5, bpm, bpm * 2.0]
        .into_iter()
        .filter(|c| (MIN_BPM..=MAX_BPM).contains(c))
        .map(|c| (c, comb(c)))
        .collect();

    // The strongest raw periodicity is the safe anchor.
    let anchor = scored.iter().copied().fold(f32::MIN, |m, (_, c)| m.max(c));

    // If nothing autocorrelates well (sparse, rubato, near-beatless material) the octave picture is
    // noise - keep the mode of the per-window estimates rather than gambling on a doubling.
    let chosen = if anchor < MIN_COMB_CONFIDENCE {
        bpm
    } else {
        // Otherwise move off the anchor only for an octave that is *nearly as periodic* (so the
        // pulse is really there, not an artefact), and among those let the perceptual tempo weight
        // pick - which is what nudges a genuine double-time track up to the tempo it's normally
        // cataloged at.
        scored
            .iter()
            .filter(|(_, c)| *c >= OCTAVE_COMB_RATIO * anchor)
            .max_by(|a, b| tempo_weight(a.0).total_cmp(&tempo_weight(b.0)))
            .map_or(bpm, |(c, _)| *c)
    };

    bpm_debug(|| {
        let rows: Vec<String> = scored
            .iter()
            .map(|(c, comb)| format!("{c:.1}bpm comb={comb:.3} w={:.3}", tempo_weight(*c)))
            .collect();
        format!(
            "resolve_octave(agg={bpm:.1}) anchor={anchor:.3} -> {chosen:.1}  [{}]",
            rows.join(", ")
        )
    });

    chosen
}

/// A faster/slower octave must reach at least this fraction of the best raw comb salience before
/// the perceptual weight is allowed to pick it over the strongest-periodicity anchor.
const OCTAVE_COMB_RATIO: f32 = 0.80;

/// Below this raw comb salience the onset envelope has no tempo worth arguing the octave of.
const MIN_COMB_CONFIDENCE: f32 = 0.15;

/// Emit a debug line (lazily formatted) when `NCSPOT_BPM_DEBUG` is set in the environment.
fn bpm_debug(f: impl FnOnce() -> String) {
    if std::env::var_os("NCSPOT_BPM_DEBUG").is_some() {
        eprintln!("[bpm] {}", f());
    }
}

/// Autocorrelation of `centred` (already mean-subtracted) at a possibly fractional `lag`, via
/// linear interpolation between the two neighbouring integer lags.
fn autocorr_at(centred: &[f32], lag: f32) -> f32 {
    let l0 = lag.floor() as usize;
    let frac = lag - l0 as f32;
    let at = |l: usize| -> f32 {
        if l >= centred.len() {
            return 0.0;
        }
        let n = centred.len() - l;
        (0..n).map(|i| centred[i] * centred[i + l]).sum::<f32>() / n as f32
    };
    at(l0) * (1.0 - frac) + at(l0 + 1) * frac
}

/// Tempo of a single window: autocorrelation, harmonic comb scoring, log-normal prior, parabolic
/// interpolation on the winning lag for a fractional BPM.
fn window_tempo(window: &[f32], min_lag: usize, max_lag: usize) -> Option<f32> {
    let max_lag = max_lag.min(window.len() / 2);
    if max_lag <= min_lag {
        return None;
    }

    let (mean, _) = mean_std(window);
    let centred: Vec<f32> = window.iter().map(|&x| x - mean).collect();

    let mut acf = vec![0.0_f32; max_lag + 2];
    for (lag, slot) in acf.iter_mut().enumerate() {
        let mut sum = 0.0_f32;
        for i in 0..centred.len() - lag {
            sum += centred[i] * centred[i + lag];
        }
        *slot = sum / (centred.len() - lag) as f32;
    }
    let zero = acf[0];
    if zero <= f32::EPSILON {
        return None;
    }
    for v in &mut acf {
        *v /= zero;
    }

    let mut best_lag = 0usize;
    let mut best_score = f32::MIN;
    for lag in min_lag..=max_lag {
        // Comb filter: average the autocorrelation at this lag and its harmonics. A subharmonic
        // (half tempo) only scores well if energy is *also* present at 2x, 3x... its lag, which
        // for a real beat it isn't, so the true tempo wins.
        let mut acc = 0.0_f32;
        let mut count = 0.0_f32;
        for h in 1..=COMB_HARMONICS {
            let hl = lag * h;
            if hl > max_lag {
                break;
            }
            acc += acf[hl];
            count += 1.0;
        }
        let bpm = 60.0 * ONSET_RATE / lag as f32;
        // A gentle preference only - the final octave is decided later by `resolve_octave`; here
        // it just nudges ambiguous windows towards a musical range.
        let prior = (-0.5 * ((bpm / TEMPO_CENTRE).ln() / 0.9).powi(2)).exp();
        let score = (acc / count) * prior;
        if score > best_score {
            best_score = score;
            best_lag = lag;
        }
    }

    if best_lag == 0 || best_score <= 0.0 {
        return None;
    }

    let refined = parabolic_peak(&acf, best_lag);
    Some(60.0 * ONSET_RATE / refined)
}

/// Reduce per-window tempo estimates to a single value: bucket to the nearest BPM, take the most
/// common bucket (ties broken towards 120), then average the raw estimates that fall in it.
fn aggregate(estimates: &[f32]) -> f32 {
    let mut counts: std::collections::HashMap<i32, usize> = std::collections::HashMap::new();
    for &b in estimates {
        *counts.entry(b.round() as i32).or_default() += 1;
    }
    let mode = counts
        .into_iter()
        .max_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| (b.0 - 120).abs().cmp(&(a.0 - 120).abs()))
        })
        .map(|(bpm, _)| bpm)
        .unwrap_or(120);

    let (sum, n) = estimates
        .iter()
        .filter(|&&b| (b.round() as i32 - mode).abs() <= 1)
        .fold((0.0_f32, 0.0_f32), |(s, n), &b| (s + b, n + 1.0));
    if n > 0.0 { sum / n } else { mode as f32 }
}

/// The original, deliberately-minimal estimator, kept for experimentation and A/B comparison
/// against the current pipeline. Not wired into anything.
///
/// One pass, no frills: plain linear-magnitude spectral flux for the onset envelope (single band,
/// no SuperFlux max-filter, no per-band normalisation), then a single autocorrelation over the
/// whole envelope, peak-picked with a log-normal preference around 120 BPM. No harmonic comb, no
/// windowing, no octave resolution - so it half/double-times readily on backbeat-driven tracks.
#[allow(dead_code)]
fn baseline_bpm(signal: &[f32]) -> Option<f32> {
    let mut planner = FftPlanner::<f32>::new();
    let fft: Arc<dyn Fft<f32>> = planner.plan_fft_forward(FFT_SIZE);
    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|n| {
            (std::f32::consts::PI * n as f32 / FFT_SIZE as f32)
                .sin()
                .powi(2)
        })
        .collect();

    let bins = FFT_SIZE / 2 + 1;
    let mut prev_mag = vec![0.0_f32; bins];
    let mut scratch = vec![Complex::<f32>::default(); FFT_SIZE];
    let mut onset = Vec::with_capacity(signal.len() / HOP);

    let mut pos = 0;
    while pos + FFT_SIZE <= signal.len() {
        for ((s, &x), &w) in scratch
            .iter_mut()
            .zip(&signal[pos..pos + FFT_SIZE])
            .zip(&window)
        {
            *s = Complex::new(x * w, 0.0);
        }
        fft.process(&mut scratch);

        let mut flux = 0.0_f32;
        for (mag_prev, bin) in prev_mag.iter_mut().zip(&scratch) {
            let mag = bin.norm();
            let diff = mag - *mag_prev;
            if diff > 0.0 {
                flux += diff;
            }
            *mag_prev = mag;
        }
        onset.push(flux);
        pos += HOP;
    }
    detrend(&mut onset, (ONSET_RATE * 0.5) as usize);

    if onset.len() < 128 {
        return None;
    }
    let min_lag = (60.0 * ONSET_RATE / MAX_BPM).floor() as usize;
    let max_lag = ((60.0 * ONSET_RATE / MIN_BPM).ceil() as usize).min(onset.len() / 2);
    if max_lag <= min_lag {
        return None;
    }

    let mut best_bpm = 0.0_f32;
    let mut best_score = f32::MIN;
    for lag in min_lag..=max_lag {
        let norm = (0..onset.len() - lag)
            .map(|i| onset[i] * onset[i + lag])
            .sum::<f32>()
            / (onset.len() - lag) as f32;
        let bpm = 60.0 * ONSET_RATE / lag as f32;
        let prior = (-0.5 * ((bpm / 120.0).ln() / 0.9).powi(2)).exp();
        let score = norm * prior;
        if score > best_score {
            best_score = score;
            best_bpm = bpm;
        }
    }

    (best_score > 0.0 && (MIN_BPM..=MAX_BPM).contains(&best_bpm)).then(|| best_bpm.round())
}

/// Parabolic interpolation of the peak position around integer index `i` of `data`, for
/// sub-sample precision. Falls back to `i` at the array edges.
fn parabolic_peak(data: &[f32], i: usize) -> f32 {
    if i == 0 || i + 1 >= data.len() {
        return i as f32;
    }
    let (a, b, c) = (data[i - 1], data[i], data[i + 1]);
    let denom = a - 2.0 * b + c;
    if denom.abs() < f32::EPSILON {
        return i as f32;
    }
    i as f32 + 0.5 * (a - c) / denom
}

// --- small numeric helpers -----------------------------------------------------------------

/// Mean and (population) standard deviation of `data`. Returns `(0, 0)` for an empty slice.
fn mean_std(data: &[f32]) -> (f32, f32) {
    if data.is_empty() {
        return (0.0, 0.0);
    }
    let n = data.len() as f32;
    let mean = data.iter().sum::<f32>() / n;
    let var = data.iter().map(|&x| (x - mean).powi(2)).sum::<f32>() / n;
    (mean, var.sqrt())
}

/// Subtract a centred moving average of half-width `w` from `data`, clamping negatives to zero.
fn detrend(data: &mut [f32], w: usize) {
    if w == 0 || data.is_empty() {
        return;
    }
    let n = data.len();
    let prefix: Vec<f32> = std::iter::once(0.0)
        .chain(data.iter().scan(0.0, |acc, &x| {
            *acc += x;
            Some(*acc)
        }))
        .collect();
    for (i, value) in data.iter_mut().enumerate() {
        let lo = i.saturating_sub(w);
        let hi = (i + w + 1).min(n);
        let mean = (prefix[hi] - prefix[lo]) / (hi - lo) as f32;
        *value = (*value - mean).max(0.0);
    }
}

// --- Subfile ------------------------------------------------------------------------------------

/// A read-only window into `stream` starting at `offset`, `length` bytes long, with positions
/// reported relative to `offset`. Reimplemented from librespot's private `player::Subfile` so
/// symphonia sees the Vorbis data without Spotify's leading header.
struct Subfile<T: Read + Seek> {
    stream: T,
    offset: u64,
    length: u64,
}

impl<T: Read + Seek> Subfile<T> {
    fn new(mut stream: T, offset: u64, length: u64) -> io::Result<Self> {
        stream.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            stream,
            offset,
            length,
        })
    }
}

impl<T: Read + Seek> Read for Subfile<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl<T: Read + Seek> Seek for Subfile<T> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let pos = match pos {
            SeekFrom::Start(offset) => SeekFrom::Start(offset + self.offset),
            other => other,
        };
        let newpos = self.stream.seek(pos)?;
        Ok(newpos.saturating_sub(self.offset))
    }
}

impl<T> MediaSource for Subfile<T>
where
    T: Read + Seek + Send + Sync,
{
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic percussive signal: a short decaying noise burst every beat at `bpm`, over
    /// `seconds`, plus a quiet sine "bass" and broadband noise so the onset detector has to work
    /// for its estimate rather than seeing clean impulses.
    fn click_track(bpm: f32, seconds: f32) -> Vec<f32> {
        let sr = SAMPLE_RATE;
        let n = (sr * seconds) as usize;
        let beat = (sr * 60.0 / bpm) as usize;
        // Deterministic LCG so the test doesn't need a dependency.
        let mut rng: u32 = 0x1234_5678;
        let mut noise = || {
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (rng >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0
        };
        (0..n)
            .map(|i| {
                let into_beat = i % beat;
                let env = (-(into_beat as f32) / (sr * 0.05)).exp();
                let click = env * noise() * 0.9;
                let bass = (2.0 * std::f32::consts::PI * 55.0 * i as f32 / sr).sin() * 0.05;
                click + bass + noise() * 0.01
            })
            .collect()
    }

    fn detect(bpm: f32) -> f32 {
        let signal = click_track(bpm, 24.0);
        let onset = onset_envelope(&signal);
        estimate_tempo(&onset).expect("an estimate")
    }

    #[test]
    fn estimates_common_tempos_within_two_bpm() {
        for &target in &[100.0_f32, 128.0, 140.0] {
            let got = detect(target);
            assert!(
                (got - target).abs() <= 2.0,
                "expected ~{target} BPM, got {got}"
            );
        }
    }

    #[test]
    fn does_not_land_on_half_or_double_time() {
        // 75 BPM is a case where a naive autocorrelation peak-pick tends to report 150.
        let got = detect(75.0);
        assert!((got - 75.0).abs() <= 2.0, "expected ~75 BPM, got {got}");
    }

    /// Decode `NCSPOT_BPM_FILE` to a mono f32 signal (first [`ANALYSIS_SAMPLES`]) via `ffmpeg`.
    fn decode_test_file() -> Vec<f32> {
        let path = std::env::var("NCSPOT_BPM_FILE").expect("set NCSPOT_BPM_FILE");
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i", &path, "-ac", "1", "-ar"])
            .arg(format!("{}", SAMPLE_RATE as u32))
            .args(["-f", "f32le", "-"])
            .output()
            .expect("run ffmpeg");
        assert!(
            out.status.success(),
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&b| f32::from_le_bytes(b))
            .take(ANALYSIS_SAMPLES)
            .collect()
    }

    /// Run the current estimator over a real audio file, for manual checking against a known BPM.
    /// Needs `ffmpeg` on PATH and the file path in `NCSPOT_BPM_FILE`; prints the estimate. E.g.:
    /// `NCSPOT_BPM_FILE=song.mp3 cargo test --bin ncspot -- --ignored --nocapture analyse_local`
    #[test]
    #[ignore = "needs NCSPOT_BPM_FILE and ffmpeg"]
    fn analyse_local_file() {
        let signal = decode_test_file();
        let onset = onset_envelope(&signal);
        println!("estimated BPM: {:?}", estimate_tempo(&onset));
    }

    /// Same, but with the original [`baseline_bpm`] estimator, for A/B comparison.
    #[test]
    #[ignore = "needs NCSPOT_BPM_FILE and ffmpeg"]
    fn analyse_local_file_baseline() {
        let signal = decode_test_file();
        println!("baseline BPM: {:?}", baseline_bpm(&signal));
    }

    #[test]
    fn parabolic_peak_interpolates_towards_the_taller_neighbour() {
        // Peak at index 2, slightly skewed towards index 3.
        let data = [0.0, 0.4, 1.0, 0.6, 0.1];
        let p = parabolic_peak(&data, 2);
        assert!(p > 2.0 && p < 2.5, "got {p}");
    }
}
