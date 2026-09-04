use std::collections::HashMap;

use crate::command::{Command, MoveAmount, MoveMode, SeekDirection};

/// Keybinding deltas this fork layers on top of [`CommandManager::default_keybindings`].
///
/// These are applied via `HashMap::extend` after the upstream defaults, so any key
/// present here overrides the upstream binding for that key (later wins).
pub(crate) fn fork_keybindings() -> HashMap<String, Vec<Command>> {
    let mut kb = HashMap::new();

    // Overrides of upstream default bindings.
    kb.insert("q".into(), vec![Command::Queue]);
    kb.insert("Shift+q".into(), vec![Command::Quit]);
    kb.insert("p".into(), vec![Command::TogglePlay]);
    kb.insert("Space".into(), vec![Command::TogglePlay]);
    kb.insert(
        "<".into(),
        vec![
            Command::Previous,
            Command::Move(MoveMode::Playing, Default::default()),
        ],
    );
    kb.insert(
        ">".into(),
        vec![
            Command::Next,
            Command::Move(MoveMode::Playing, Default::default()),
        ],
    );
    kb.insert("Backspace".into(), vec![Command::RemoveFromPlaylists]);
    kb.insert(
        "Shift+p".into(),
        vec![Command::Move(MoveMode::Playing, Default::default())],
    );
    kb.insert(
        "Shift+k".into(),
        vec![Command::Move(MoveMode::Up, MoveAmount::Integer(5))],
    );
    kb.insert(
        "Shift+j".into(),
        vec![Command::Move(MoveMode::Down, MoveAmount::Integer(5))],
    );
    kb.insert(
        "PageUp".into(),
        vec![Command::Move(MoveMode::Up, MoveAmount::Float(1.0))],
    );
    kb.insert(
        "PageDown".into(),
        vec![Command::Move(MoveMode::Down, MoveAmount::Float(1.0))],
    );

    // Additions (keys upstream leaves unbound).
    kb.insert("1".into(), vec![Command::Play]);
    kb.insert(
        "2".into(),
        vec![
            Command::Previous,
            Command::Move(MoveMode::Playing, Default::default()),
        ],
    );
    kb.insert(
        "3".into(),
        vec![
            Command::Next,
            Command::Move(MoveMode::Playing, Default::default()),
        ],
    );
    kb.insert("`".into(), vec![Command::BindKey]);
    kb.insert("'".into(), vec![Command::ToggleSortFilter]);
    kb.insert(
        "Shift+Right".into(),
        vec![Command::Seek(SeekDirection::Relative(10000))],
    );
    kb.insert(
        "Shift+Left".into(),
        vec![Command::Seek(SeekDirection::Relative(-10000))],
    );

    kb
}
