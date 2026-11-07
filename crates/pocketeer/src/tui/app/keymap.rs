//! Which key does what in each state of the screen, and how the bar at the
//! bottom and the help window show it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, Dialog, ImportTarget, Page};
use crate::tui::worker::WorkerCommand;

/// Where a key is shown.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// The bar's row of keys that change the library.
    Library,
    /// The bar's row of keys that change the device.
    Device,
    /// The bar's row of other keys.
    General,
    /// The cover panel, while the game has no cover.
    Cover,
    /// Nowhere. The help window's notes describe these.
    Hidden,
}

/// A key as it is pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Key {
    const fn plain(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    const fn ctrl(character: char) -> Self {
        Self {
            code: KeyCode::Char(character),
            modifiers: KeyModifiers::CONTROL,
        }
    }

    /// Shift is ignored, because some terminals report it with a capital
    /// letter and others don't.
    fn matches(&self, event: KeyEvent) -> bool {
        event.code == self.code && event.modifiers.difference(KeyModifiers::SHIFT) == self.modifiers
    }
}

/// One key of a keymap: what it matches, how the bar and the help window
/// show it, and what it does.
#[derive(Clone, Copy)]
pub struct Binding {
    keys: &'static [Key],
    /// What the keycap says, such as "j", "↵" or "↑↓".
    pub keycap: &'static str,
    /// The word after the keycap in the bar.
    pub short_desc: &'static str,
    /// The sentence in the help window.
    pub long_desc: &'static str,
    pub group: Group,
    action: fn(&mut App),
}

impl Binding {
    /// Fills the slots of a joined array before they are written.
    const NONE: Self = Self::hidden(&[], |_| {});

    const fn new(
        keys: &'static [Key],
        keycap: &'static str,
        short_desc: &'static str,
        long_desc: &'static str,
        group: Group,
        action: fn(&mut App),
    ) -> Self {
        Self {
            keys,
            keycap,
            short_desc,
            long_desc,
            group,
            action,
        }
    }

    const fn hidden(keys: &'static [Key], action: fn(&mut App)) -> Self {
        Self::new(keys, "", "", "", Group::Hidden, action)
    }
}

/// The keys of one state of the screen.
pub struct Keymap {
    pub bindings: &'static [Binding],
    /// Lines for the help window about what the bindings don't say.
    pub notes: &'static [&'static str],
}

impl Keymap {
    /// Does what `event` is bound to, if anything, and says whether it was.
    pub(super) fn press(&self, event: KeyEvent, app: &mut App) -> bool {
        let bound = self
            .bindings
            .iter()
            .find(|binding| binding.keys.iter().any(|key| key.matches(event)));
        match bound {
            Some(binding) => {
                (binding.action)(app);
                true
            }
            None => false,
        }
    }

    pub fn in_group(&self, group: Group) -> impl Iterator<Item = &Binding> {
        self.bindings
            .iter()
            .filter(move |binding| binding.group == group)
    }

    /// `parts` as one array. Its length is checked when the program is built.
    const fn join<const N: usize>(parts: &[&[Binding]]) -> [Binding; N] {
        let mut joined = [Binding::NONE; N];
        let mut at = 0;
        let mut part_index = 0;
        while part_index < parts.len() {
            let part = parts[part_index];
            let mut i = 0;
            while i < part.len() {
                assert!(at < N, "more bindings than the length says");
                joined[at] = part[i];
                at += 1;
                i += 1;
            }
            part_index += 1;
        }
        assert!(at == N, "fewer bindings than the length says");
        joined
    }

    // ---- Pieces several keymaps share -------------------------------------------

    const MOVE_NOTE: &'static str = "Move      j k, arrows, C-n C-p; PgUp PgDn; g G top, bottom";
    const PAGES_NOTE: &'static str =
        "Pages     Tab the next one; 1 games, 2 cores, 3 BIOS, 4 device";
    const STOP_NOTE: &'static str = "Stop      Esc stops what runs, after the game under way.";
    const SHIFT_NOTE: &'static str =
        "          Lowercase changes the library, Shift changes the device.";

    const LIST_MOVEMENT: [Binding; 6] = [
        Binding::hidden(
            &[
                Key::plain(KeyCode::Down),
                Key::plain(KeyCode::Char('j')),
                Key::ctrl('n'),
            ],
            |app| app.move_rows(1),
        ),
        Binding::hidden(
            &[
                Key::plain(KeyCode::Up),
                Key::plain(KeyCode::Char('k')),
                Key::ctrl('p'),
            ],
            |app| app.move_rows(-1),
        ),
        Binding::hidden(&[Key::plain(KeyCode::PageDown)], |app| {
            app.move_rows(App::PAGE_KEY_ROW_COUNT)
        }),
        Binding::hidden(&[Key::plain(KeyCode::PageUp)], |app| {
            app.move_rows(-App::PAGE_KEY_ROW_COUNT)
        }),
        Binding::hidden(
            &[Key::plain(KeyCode::Home), Key::plain(KeyCode::Char('g'))],
            |app| app.select_row(0),
        ),
        Binding::hidden(
            &[Key::plain(KeyCode::End), Key::plain(KeyCode::Char('G'))],
            |app| app.select_row(usize::MAX),
        ),
    ];

    const PAGES: [Binding; 5] = [
        Binding::hidden(&[Key::plain(KeyCode::Tab)], App::next_page),
        Binding::hidden(&[Key::plain(KeyCode::Char('1'))], |app| {
            app.current_page = Page::Games
        }),
        Binding::hidden(&[Key::plain(KeyCode::Char('2'))], App::show_cores),
        Binding::hidden(&[Key::plain(KeyCode::Char('3'))], |app| {
            app.current_page = Page::Bios
        }),
        Binding::hidden(&[Key::plain(KeyCode::Char('4'))], |app| {
            app.current_page = Page::Device
        }),
    ];

    const SHARED: [Binding; 3] = [
        Binding::new(
            &[Key::plain(KeyCode::Char('r'))],
            "r",
            "reload",
            "Reads the library and the device again.",
            Group::General,
            |app| app.worker_handle.send(WorkerCommand::Reload),
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('?'))],
            "?",
            "help",
            "This window.",
            Group::General,
            |app| app.dialog = Some(Dialog::keys(app.keymap())),
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('q'))],
            "q",
            "quit",
            "Quits. What is under way finishes first.",
            Group::General,
            |app| app.should_quit = true,
        ),
    ];

    /// The status line shows this key while something runs.
    const STOP: [Binding; 1] = [Binding::hidden(&[Key::plain(KeyCode::Esc)], App::escape)];

    // ---- The Games page ---------------------------------------------------------

    const GAMES_ACTIONS: [Binding; 18] = [
        Binding::new(
            &[Key::plain(KeyCode::Char('i'))],
            "i",
            "import",
            "Imports a game, or every game in a folder, from this computer.",
            Group::Library,
            |app| app.prompt_for_import_path(ImportTarget::Game),
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('p'))],
            "p",
            "pull",
            "Copies the game from the device into the library.",
            Group::Library,
            |app| app.send_for_device_rows(WorkerCommand::PullGames),
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('s'))],
            "s",
            "save",
            "Backs up the device's save as a new version in the library.",
            Group::Library,
            |app| app.send_for_device_rows(WorkerCommand::PullSaves),
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('n'))],
            "n",
            "rename",
            "Renames the game in the library. Its saves and cover go with it.",
            Group::Library,
            App::prompt_for_name_in_library,
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('t'))],
            "t",
            "standard",
            "Renames the game to what the DATs call it. It asks first.",
            Group::Library,
            App::propose_renames_to_standard_names,
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('x'))],
            "x",
            "trash",
            "Trashes the game with its cover, or an orphan save. It asks first.",
            Group::Library,
            App::confirm_trash_games,
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('P'))],
            "P",
            "push",
            "Copies the game from the library onto the device.",
            Group::Device,
            |app| app.send_for_library_rows(WorkerCommand::PushGames),
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('N'))],
            "N",
            "rename",
            "Renames the game on the device. Its save goes with it.",
            Group::Device,
            App::prompt_for_name_on_device,
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('E'))],
            "E",
            "eject",
            "Ejects the device.",
            Group::Device,
            App::eject,
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('X'))],
            "X",
            "delete",
            "Deletes the game or an orphan save for good. It asks first.",
            Group::Device,
            App::confirm_delete_games,
        ),
        Binding::new(
            &[Key::plain(KeyCode::Enter), Key::plain(KeyCode::Right)],
            "↵",
            "saves",
            "Opens the versions of the game's save.",
            Group::General,
            |app| app.games.open_saves_panel(),
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('m'))],
            "m",
            "mark",
            "Marks the row. Keys act on the marked rows, or on the selected one.",
            Group::General,
            |app| app.games.toggle_mark_and_move_on(),
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('d'))],
            "d",
            "DATs",
            "Downloads the DATs that changed. They name games.",
            Group::General,
            |app| app.worker_handle.send(WorkerCommand::SyncDats),
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('a'))],
            "a",
            "download it",
            "Downloads a cover for the game.",
            Group::Cover,
            |app| app.send_for_library_rows(WorkerCommand::FetchCovers),
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('c'))],
            "c",
            "use an image file",
            "Makes an image file of yours its cover.",
            Group::Cover,
            App::prompt_for_cover_to_import,
        ),
        Binding::hidden(&[Key::plain(KeyCode::Char('M'))], |app| {
            app.games.mark_all_shown_or_clear_marks()
        }),
        Binding::hidden(&[Key::plain(KeyCode::Char('/'))], |app| {
            app.games.start_search()
        }),
        Binding::hidden(&[Key::plain(KeyCode::Char('f'))], |app| {
            app.games.show_next_filter()
        }),
    ];

    const GAMES_PAGE_BINDINGS: [Binding; 33] = Self::join(&[
        &Self::GAMES_ACTIONS,
        &Self::SHARED,
        &Self::STOP,
        &Self::LIST_MOVEMENT,
        &Self::PAGES,
    ]);

    pub(super) const GAMES_PAGE: Self = Self {
        bindings: &Self::GAMES_PAGE_BINDINGS,
        notes: &[
            Self::MOVE_NOTE,
            Self::PAGES_NOTE,
            Self::STOP_NOTE,
            "Mark      M marks every row shown, and again clears every mark.",
            Self::SHIFT_NOTE,
            "Search    / starts a search, Esc clears it; f cycles the filter.",
        ],
    };

    pub(super) const SEARCH: Self = Self {
        bindings: &[
            Binding::new(
                &[Key::plain(KeyCode::Enter)],
                "↵",
                "done",
                "",
                Group::General,
                |app| app.games.finish_search(),
            ),
            Binding::new(
                &[Key::plain(KeyCode::Esc)],
                "Esc",
                "clear",
                "",
                Group::General,
                |app| app.games.clear_search(),
            ),
            Binding::hidden(&[Key::plain(KeyCode::Down), Key::ctrl('n')], |app| {
                app.games.finish_search();
                app.move_rows(1);
            }),
            Binding::hidden(&[Key::plain(KeyCode::Up), Key::ctrl('p')], |app| {
                app.games.finish_search();
                app.move_rows(-1);
            }),
            Binding::hidden(&[Key::plain(KeyCode::PageDown)], |app| {
                app.games.finish_search();
                app.move_rows(App::PAGE_KEY_ROW_COUNT);
            }),
            Binding::hidden(&[Key::plain(KeyCode::PageUp)], |app| {
                app.games.finish_search();
                app.move_rows(-App::PAGE_KEY_ROW_COUNT);
            }),
        ],
        notes: &[],
    };

    pub(super) const SAVES_PANEL: Self = Self {
        bindings: &[
            Binding::new(
                &[Key::plain(KeyCode::Char('i'))],
                "i",
                "import",
                "Imports a file of yours as a new version of the save.",
                Group::Library,
                App::prompt_for_save_to_import,
            ),
            Binding::new(
                &[Key::plain(KeyCode::Char('s'))],
                "s",
                "save",
                "Backs up the device's save as a new version.",
                Group::Library,
                |app| app.send_for_device_rows(WorkerCommand::PullSaves),
            ),
            Binding::new(
                &[Key::plain(KeyCode::Char('x'))],
                "x",
                "trash",
                "Trashes the selected version. It asks first.",
                Group::Library,
                App::confirm_trash_selected_version,
            ),
            Binding::new(
                &[Key::plain(KeyCode::Char('S'))],
                "S",
                "push",
                "Pushes the version onto the device, if the device's save is backed up.",
                Group::Device,
                App::push_selected_version,
            ),
            Binding::new(
                &[Key::plain(KeyCode::Char('X'))],
                "X",
                "delete",
                "Deletes the device's save for good. It asks first.",
                Group::Device,
                App::confirm_delete_save,
            ),
            Binding::new(
                &[
                    Key::plain(KeyCode::Esc),
                    Key::plain(KeyCode::Left),
                    Key::plain(KeyCode::Char('h')),
                ],
                "Esc",
                "back",
                "Back to the list.",
                Group::General,
                |app| app.games.close_saves_panel(),
            ),
            Self::SHARED[1],
            Self::SHARED[2],
            Binding::hidden(
                &[
                    Key::plain(KeyCode::Down),
                    Key::plain(KeyCode::Char('j')),
                    Key::ctrl('n'),
                ],
                |app| app.games.move_in_saves_panel(1),
            ),
            Binding::hidden(
                &[
                    Key::plain(KeyCode::Up),
                    Key::plain(KeyCode::Char('k')),
                    Key::ctrl('p'),
                ],
                |app| app.games.move_in_saves_panel(-1),
            ),
        ],
        notes: &[Self::SHIFT_NOTE],
    };

    // ---- The other pages --------------------------------------------------------

    const CORES_ACTIONS: [Binding; 3] = [
        Binding::new(
            &[Key::plain(KeyCode::Char('U'))],
            "U",
            "install or update",
            "Puts the newest release of the core on the device.",
            Group::Device,
            App::install_selected_core,
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('A'))],
            "A",
            "update all",
            "Updates every core that has a newer release.",
            Group::Device,
            App::update_every_core,
        ),
        Binding::new(
            &[Key::plain(KeyCode::Char('X'))],
            "X",
            "delete",
            "Deletes the core for good. Its settings and saves stay. It asks first.",
            Group::Device,
            App::confirm_remove_core,
        ),
    ];

    const CORES_PAGE_BINDINGS: [Binding; 18] = Self::join(&[
        &Self::CORES_ACTIONS,
        &Self::SHARED,
        &Self::STOP,
        &Self::LIST_MOVEMENT,
        &Self::PAGES,
    ]);

    pub(super) const CORES_PAGE: Self = Self {
        bindings: &Self::CORES_PAGE_BINDINGS,
        notes: &[Self::MOVE_NOTE, Self::PAGES_NOTE, Self::STOP_NOTE],
    };

    const BIOS_ACTIONS: [Binding; 1] = [Binding::new(
        &[Key::plain(KeyCode::Char('I'))],
        "I",
        "put a file",
        "Copies a file of yours to where the core loads it from.",
        Group::Device,
        App::prompt_for_bios_file,
    )];

    const BIOS_PAGE_BINDINGS: [Binding; 16] = Self::join(&[
        &Self::BIOS_ACTIONS,
        &Self::SHARED,
        &Self::STOP,
        &Self::LIST_MOVEMENT,
        &Self::PAGES,
    ]);

    pub(super) const BIOS_PAGE: Self = Self {
        bindings: &Self::BIOS_PAGE_BINDINGS,
        notes: &[Self::MOVE_NOTE, Self::PAGES_NOTE, Self::STOP_NOTE],
    };

    const DEVICE_PAGE_BINDINGS: [Binding; 9] =
        Self::join(&[&Self::SHARED, &Self::STOP, &Self::PAGES]);

    pub(super) const DEVICE_PAGE: Self = Self {
        bindings: &Self::DEVICE_PAGE_BINDINGS,
        notes: &[Self::PAGES_NOTE, Self::STOP_NOTE],
    };

    // ---- Prompts and dialogs ----------------------------------------------------

    // While typing, `?` is a character, so these have no explanations.

    /// The keys of a path prompt after Enter, which differs by what it does.
    const PATH_PROMPT_REST: [Binding; 4] = [
        Binding::new(
            &[Key::plain(KeyCode::Esc)],
            "Esc",
            "cancel",
            "",
            Group::General,
            App::cancel_import,
        ),
        Binding::new(
            &[Key::plain(KeyCode::Tab)],
            "Tab",
            "complete",
            "",
            Group::General,
            App::complete_path,
        ),
        Binding::new(
            &[Key::plain(KeyCode::Down), Key::ctrl('n')],
            "↑↓",
            "pick",
            "",
            Group::General,
            App::pick_next_path,
        ),
        Binding::hidden(
            &[Key::plain(KeyCode::Up), Key::ctrl('p')],
            App::pick_previous_path,
        ),
    ];

    const IMPORT_PROMPT_BINDINGS: [Binding; 5] = Self::join(&[
        &[Binding::new(
            &[Key::plain(KeyCode::Enter)],
            "↵",
            "import",
            "",
            Group::General,
            App::confirm_import,
        )],
        &Self::PATH_PROMPT_REST,
    ]);

    pub(super) const IMPORT_PROMPT: Self = Self {
        bindings: &Self::IMPORT_PROMPT_BINDINGS,
        notes: &[],
    };

    const BIOS_PROMPT_BINDINGS: [Binding; 5] = Self::join(&[
        &[Binding::new(
            &[Key::plain(KeyCode::Enter)],
            "↵",
            "put on the device",
            "",
            Group::General,
            App::confirm_import,
        )],
        &Self::PATH_PROMPT_REST,
    ]);

    pub(super) const BIOS_PROMPT: Self = Self {
        bindings: &Self::BIOS_PROMPT_BINDINGS,
        notes: &[],
    };

    pub(super) const PLATFORM_PROMPT: Self = Self {
        bindings: &[
            Binding::new(
                &[Key::plain(KeyCode::Enter)],
                "↵",
                "import",
                "",
                Group::General,
                App::confirm_platform,
            ),
            Binding::new(
                &[Key::plain(KeyCode::Esc)],
                "Esc",
                "cancel",
                "",
                Group::General,
                |app| app.prompt = None,
            ),
            Binding::new(
                &[
                    Key::plain(KeyCode::Down),
                    Key::plain(KeyCode::Char('j')),
                    Key::ctrl('n'),
                ],
                "↑↓",
                "pick",
                "",
                Group::General,
                |app| app.pick_platform(1),
            ),
            Binding::hidden(
                &[
                    Key::plain(KeyCode::Up),
                    Key::plain(KeyCode::Char('k')),
                    Key::ctrl('p'),
                ],
                |app| app.pick_platform(-1),
            ),
        ],
        notes: &[],
    };

    pub(super) const RENAME_PROMPT: Self = Self {
        bindings: &[
            Binding::new(
                &[Key::plain(KeyCode::Enter)],
                "↵",
                "rename",
                "",
                Group::General,
                App::confirm_rename,
            ),
            Binding::new(
                &[Key::plain(KeyCode::Esc)],
                "Esc",
                "cancel",
                "",
                Group::General,
                |app| app.prompt = None,
            ),
        ],
        notes: &[],
    };

    pub(super) const DIALOG: Self = Self {
        bindings: &[
            Binding::new(
                &[Key::plain(KeyCode::Enter), Key::plain(KeyCode::Char('y'))],
                "↵",
                "choose",
                "",
                Group::General,
                App::dialog_confirm,
            ),
            Binding::new(
                &[
                    Key::plain(KeyCode::Esc),
                    Key::plain(KeyCode::Char('n')),
                    Key::plain(KeyCode::Char('q')),
                    Key::plain(KeyCode::Char('?')),
                ],
                "Esc",
                "cancel",
                "",
                Group::General,
                App::dialog_cancel,
            ),
            Binding::new(
                &[Key::plain(KeyCode::Left), Key::plain(KeyCode::Char('h'))],
                "←→",
                "move",
                "",
                Group::General,
                App::dialog_focus_left,
            ),
            Binding::hidden(
                &[
                    Key::plain(KeyCode::Right),
                    Key::plain(KeyCode::Char('l')),
                    Key::plain(KeyCode::Tab),
                ],
                App::dialog_focus_right,
            ),
        ],
        notes: &[],
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_each_key_once_per_keymap() {
        for keymap in [
            &Keymap::GAMES_PAGE,
            &Keymap::SEARCH,
            &Keymap::SAVES_PANEL,
            &Keymap::CORES_PAGE,
            &Keymap::BIOS_PAGE,
            &Keymap::DEVICE_PAGE,
            &Keymap::IMPORT_PROMPT,
            &Keymap::BIOS_PROMPT,
            &Keymap::PLATFORM_PROMPT,
            &Keymap::RENAME_PROMPT,
            &Keymap::DIALOG,
        ] {
            let mut seen: Vec<Key> = Vec::new();
            for key in keymap.bindings.iter().flat_map(|binding| binding.keys) {
                assert!(!seen.contains(key), "{key:?} is bound twice");
                seen.push(*key);
            }
        }
    }
}
