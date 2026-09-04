use std::sync::Arc;

use cursive::Cursive;
use cursive::event::{Event, EventResult, Key};
use cursive::traits::{Finder, Nameable};
use cursive::view::{Margins, View, ViewWrapper};
use cursive::views::{Dialog, EditView, NamedView, ScrollView, SelectView};
use cursive::wrap_impl;

use crate::application::UserData;
use crate::command::Command;
use crate::commands::{CommandManager, CommandResult};
use crate::ext_traits::SelectViewExt;
use crate::library::Library;
use crate::model::playlist::Playlist;
use crate::traits::ViewExt;
use crate::ui::layout::Layout;
use crate::ui::modal::Modal;

pub struct BindKeyMenu {
    dialog: Modal<Dialog>,
    library: Arc<Library>,
}

impl BindKeyMenu {
    pub fn bind_key_dialog(library: Arc<Library>) -> NamedView<Self> {
        Self {
            dialog: Modal::new_ext(Self::bind_key_playlist_dialog(library.clone())),
            library,
        }
        .with_name("bindkeymenu")
    }

    fn bind_key_playlist_dialog(library: Arc<Library>) -> Dialog {
        let mut list_select: SelectView<Playlist> = SelectView::new();
        let current_user_id = library.user_id.as_ref().unwrap();
        let bound_keys = CommandManager::bound_playlist_keys(&library.cfg);

        for list in library.playlists.read().unwrap().iter() {
            if current_user_id == &list.owner_id || list.collaborative {
                let key = bound_keys
                    .get(&list.name.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or_else(|| "-".to_string());
                list_select.add_item(format!("{:<24} {key}", list.name), list.clone());
            }
        }

        list_select.set_on_submit(move |s, selected| {
            let dialog = Self::bind_key_prompt_dialog(library.clone(), selected.clone());
            s.call_on_name("bindkeymenu", |v: &mut Self| {
                v.dialog = Modal::new_ext(dialog);
            });
            s.focus_name("bindkey_edit").ok();
        });

        Dialog::new()
            .title("Bind key to playlist (Backspace to clear)")
            .dismiss_button("Close")
            .padding(Margins::lrtb(1, 1, 1, 0))
            .content(ScrollView::new(list_select.with_name("bindkey_select")))
    }

    fn bind_key_prompt_dialog(library: Arc<Library>, playlist: Playlist) -> Dialog {
        let mut key_edit = EditView::new();
        key_edit.set_on_submit(move |s, key| {
            if !key.is_empty() {
                library
                    .cfg
                    .add_keybinding(key.to_string(), format!("addtoplaylist {}", playlist.name));
                if let Some(data) = s.user_data::<UserData>().cloned() {
                    data.cmd.handle(s, Command::ReloadConfig);
                }
                s.call_on_name("main", |v: &mut Layout| {
                    v.set_result(Ok(Some(format!(
                        "Bound \"{key}\" to playlist \"{}\"",
                        playlist.name
                    ))));
                });
            }
            s.pop_layer();
        });

        Dialog::new()
            .title("Type a key and press Enter to bind it")
            .dismiss_button("Cancel")
            .padding(Margins::lrtb(1, 1, 1, 0))
            .content(key_edit.with_name("bindkey_edit"))
    }
}

impl ViewExt for BindKeyMenu {
    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        handle_move_command::<Playlist>(&mut self.dialog, s, cmd, "bindkey_select")
    }
}

fn handle_move_command<T: Send + Sync + 'static>(
    sel: &mut Modal<Dialog>,
    s: &mut Cursive,
    cmd: &Command,
    name: &str,
) -> Result<CommandResult, String> {
    match cmd {
        Command::Back => {
            s.pop_layer();
            Ok(CommandResult::Consumed(None))
        }
        Command::Move(_, _) => sel
            .call_on_name(name, |select: &mut SelectView<T>| {
                select.handle_command(cmd)
            })
            .unwrap_or(Ok(CommandResult::Consumed(None))),
        _ => Ok(CommandResult::Consumed(None)),
    }
}

impl ViewWrapper for BindKeyMenu {
    wrap_impl!(self.dialog: Modal<Dialog>);

    fn wrap_on_event(&mut self, event: Event) -> EventResult {
        let selected_playlist = (event == Event::Key(Key::Backspace))
            .then(|| {
                self.dialog
                    .call_on_name("bindkey_select", |select: &mut SelectView<Playlist>| {
                        select.selection().map(|rc| (*rc).clone())
                    })
            })
            .flatten()
            .flatten();

        let Some(playlist) = selected_playlist else {
            return self.dialog.on_event(event);
        };

        let library = self.library.clone();
        EventResult::with_cb_once(move |s| {
            if let Some(key) = CommandManager::bound_playlist_keys(&library.cfg)
                .get(&playlist.name.to_ascii_lowercase())
                .cloned()
            {
                library.cfg.remove_keybinding(&key);
                if let Some(data) = s.user_data::<UserData>().cloned() {
                    data.cmd.handle(s, Command::ReloadConfig);
                }
                s.call_on_name("bindkeymenu", |v: &mut Self| {
                    v.dialog = Modal::new_ext(Self::bind_key_playlist_dialog(v.library.clone()));
                });
                s.call_on_name("main", |v: &mut Layout| {
                    v.set_result(Ok(Some(format!(
                        "Unbound \"{key}\" from playlist \"{}\"",
                        playlist.name
                    ))));
                });
            }
        })
    }
}
