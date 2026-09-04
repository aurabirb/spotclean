<div align="center" style="text-align:center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="images/logo_text_dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="images/logo_text_light.svg">
    <img alt="ncspot logo" height="128" src="images/logo_text_light.svg">
  </picture>
  <h3>A fork of <a href="https://github.com/hrkfdn/ncspot">ncspot</a> — an ncurses Spotify client in Rust</h3>
</div>

This is a personal fork of [ncspot](https://github.com/hrkfdn/ncspot). It tracks
upstream and adds a **Sort** workflow for quickly filing your Liked Songs into
playlists, one keypress per playlist, plus offline BPM detection and some
playback tweaks.

Requires a Spotify **premium** account. For installation, building, configuration
and the full feature list, see the [upstream documentation](https://github.com/hrkfdn/ncspot/blob/main/doc/users.md).
The fork's own additions are documented in [`doc/users.md`](doc/users.md).

---

## Usage

### Sorting Liked Songs into playlists (the point of this fork)

Bind one key to each playlist, then walk down your Liked Songs (or the **Sort**
tab in the Library screen) tapping a key to file each track.

Each playlist has a key assigned to it. The **Sort** tab (Library → **Sort**)
shows, for each track, which playlists it belongs to. To assign keys to
playlists, press <kbd>`</kbd> (backtick).

| Key | What it does |
|-----|--------------|
| <kbd>`</kbd> (backtick) | Open the menu to assign a key to a playlist |
| <kbd>1</kbd>…<kbd>9</kbd> (whatever you assign) | Toggle the selected track in/out of that playlist |
| <kbd>Backspace</kbd> | Remove the selected track from every bound playlist |
| <kbd>'</kbd> (apostrophe) | Sort tab: show/hide already-sorted tracks |

1. **Assign a key to a playlist.** Select any track, press <kbd>`</kbd>
   (backtick). A menu lists your playlists and the key each is bound to. Pick a
   playlist, then press the key you want. (Press <kbd>Backspace</kbd> on an entry
   to clear its binding.)

2. **File tracks.** With a track selected, press a playlist's key to toggle the
   track in or out of that playlist — add it if it's not there, remove it (every
   copy) if it is. Only that playlist is touched; the track's other playlists are
   left alone. So if a track is already in X, Y, Z and you press the key for T,
   it's in X, Y, Z, T; press T again and it's back to X, Y, Z.

   <kbd>Backspace</kbd> removes the selected track from *every* key-bound playlist
   at once. **Liked Songs is never touched by any of this.**

### Getting around

| Key | What it does |
|-----|--------------|
| <kbd>F1</kbd> | Queue |
| <kbd>F2</kbd> | Search |
| <kbd>F3</kbd> | Library (contains the **Sort** tab) |
| <kbd>?</kbd> | Help screen (full keybinding list) |
| <kbd>:</kbd> | Command prompt |
| <kbd>/</kbd> | Search bar (search within the current list) |
| <kbd>j</kbd> / <kbd>k</kbd> or arrows | Move selection down / up |
| <kbd>g</kbd> / <kbd>G</kbd> | Jump to top / bottom |
| <kbd>Escape</kbd> | Close the current view / prompt |
| <kbd>Shift</kbd>+<kbd>Q</kbd> | Quit |

### Playback

| Key | What it does |
|-----|--------------|
| <kbd>Return</kbd> | Play the selected track or playlist |
| <kbd>Space</kbd> or <kbd>P</kbd> | Play / pause |
| <kbd>Q</kbd> | Add the selected item to the queue |
| <kbd>.</kbd> | Play the selected track right after the current one |
| <kbd>&lt;</kbd> / <kbd>&gt;</kbd> | Previous / next track (selection follows the track) |
| <kbd>F</kbd> / <kbd>B</kbd> | Seek forward / back 1s (<kbd>Shift</kbd> for 10s) |
| <kbd>-</kbd> / <kbd>+</kbd> | Volume ±1% (<kbd>[</kbd> / <kbd>]</kbd> for ±5%) |
| <kbd>R</kbd> / <kbd>Z</kbd> | Toggle repeat / shuffle |
| <kbd>S</kbd> / <kbd>D</kbd> | Save / remove the playing track from your library |
| <kbd>Shift</kbd>+<kbd>P</kbd> | Jump to the playing track in the queue |
| <kbd>Shift</kbd>+<kbd>S</kbd> | Stop |

## BPM detection

The `%bpm` track field is filled in the first time you select a track in a list
(unless `enrich_metadata = false` in the config). Detection is entirely local:
the track's own Spotify audio stream is decoded in the background — straight from
librespot's audio cache if you've played it before — and the tempo is estimated
with a clean-room beat-tracking pipeline (multi-band SuperFlux onset envelope →
harmonic-comb autocorrelation → octave resolution). Nothing is sent to any
third-party service.

### Exploring the estimator

`src/bpm.rs` also keeps the original, much simpler estimator as `baseline_bpm`
(single-band spectral flux + one autocorrelation pass, no comb / windowing /
octave logic) for comparison. Two `#[ignore]`d tests run either estimator over a
real audio file, decoded with `ffmpeg`:

```sh
# current pipeline
NCSPOT_BPM_FILE=song.mp3 cargo test --bin ncspot -- --ignored --nocapture analyse_local_file

# original baseline, same track
NCSPOT_BPM_FILE=song.mp3 cargo test --bin ncspot -- --ignored --nocapture analyse_local_file_baseline
```

Set `NCSPOT_BPM_DEBUG=1` as well to print the per-window estimates and the
octave-resolution scoring.

## Build & run

This fork isn't packaged anywhere — you build it from source.

### 1. Install Rust

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Restart your shell afterwards (or `source "$HOME/.cargo/env"`) so `cargo` is on
your `PATH`.

### 2. Install system dependencies

**macOS** (Homebrew):

```sh
brew install pkg-config portaudio
```

**Linux** — install the dev headers for dbus, pulseaudio, ncurses, openssl and
xcb. For example:

```sh
# Debian / Ubuntu
sudo apt install build-essential libdbus-1-dev libpulse-dev libncurses-dev libssl-dev libxcb1-dev pkg-config

# Fedora
sudo dnf install dbus-devel pulseaudio-libs-devel ncurses-devel openssl-devel libxcb-devel

# Arch
sudo pacman -S dbus libpulse libxcb ncurses openssl pkgconf
```

### 3. Build and install

```sh
git clone https://github.com/aurabirb/spotclean
cd spotclean
cargo install --path . --locked
```

This puts the `ncspot` binary in `~/.cargo/bin`. To just run it from the source
tree without installing, use `cargo run --release` in place of the steps below.

### 4. Run

```sh
ncspot
```

Run with a debug log written to a file:

```sh
ncspot -d ncspot.log
```

If it crashes, the latest backtrace is at `$NCSPOT_CACHE_DIRECTORY/backtrace.log`
(run `ncspot info` to find the cache directory).
