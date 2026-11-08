//! The windows that ask a question or show a message over the screen.

use pocketeer_core::{
    device::{DeviceEntry, OrphanDeviceSave},
    library::{LibrarySave, LocalEntry, OrphanLocalSave},
};

use super::keymap::{Binding, Group, Keymap};
use crate::tui::worker::WorkerCommand;

pub struct DialogButton {
    pub label: &'static str,
    pub command: Option<WorkerCommand>,
    /// Whether it removes something, to the trash or for good. Such a button
    /// is drawn in red, and a dialog with one starts on Cancel.
    pub is_removal: bool,
}

impl DialogButton {
    fn new(label: &'static str, command: Option<WorkerCommand>) -> Self {
        Self {
            label,
            command,
            is_removal: false,
        }
    }

    fn removal(label: &'static str, command: WorkerCommand) -> Self {
        Self {
            label,
            command: Some(command),
            is_removal: true,
        }
    }
}

/// A question or message shown over the screen.
pub struct Dialog {
    pub title: String,
    pub lines: Vec<String>,
    /// The last button has no command and cancels.
    pub buttons: Vec<DialogButton>,
    pub focused_button: usize,
}

impl Dialog {
    /// The keys of what has the focus, with explanations, and what they
    /// don't say.
    pub(super) fn keys(keymap: &Keymap) -> Self {
        let mut lines = Vec::new();
        for (heading, group) in [
            ("LIBRARY", Group::Library),
            ("DEVICE", Group::Device),
            ("GENERAL", Group::General),
            ("COVER", Group::Cover),
        ] {
            let shown: Vec<&Binding> = keymap.in_group(group).collect();
            if shown.is_empty() {
                continue;
            }
            lines.push(heading.to_string());
            for binding in shown {
                lines.push(format!("  {:<5}{}", binding.keycap, binding.long_desc));
            }
        }
        lines.push(String::new());
        lines.extend(keymap.notes.iter().map(ToString::to_string));
        Self {
            title: "Keys".to_string(),
            lines,
            buttons: vec![DialogButton::new("Close", None)],
            focused_button: 0,
        }
    }

    /// Asks whether to rename games to their DAT names, showing the new names
    /// first.
    pub(super) fn renames(renames: Vec<(LocalEntry, String)>) -> Self {
        let mut lines = vec![
            "These get the names the DATs have; saves and covers go with them.".to_string(),
            "The device isn't touched.".to_string(),
            String::new(),
        ];
        // Each game takes two lines because names are long, so this shows half
        // as many as other dialogs.
        const SHOWN: usize = 5;
        for (local_entry, new) in renames.iter().take(SHOWN) {
            lines.push(local_entry.stem().to_string());
            lines.push(format!("  → {new}"));
        }
        if renames.len() > SHOWN {
            lines.push(format!("… and {} more", renames.len() - SHOWN));
        }
        Self {
            title: format!("Rename {}", Self::pluralized(renames.len(), "game")),
            lines,
            buttons: vec![
                DialogButton::new("Rename", Some(WorkerCommand::RenameGamesInLibrary(renames))),
                DialogButton::new("Cancel", None),
            ],
            focused_button: 0,
        }
    }

    pub(super) fn duplicates(duplicates: Vec<String>) -> Self {
        let mut lines = vec![
            "These copies are left out, and their saves aren't synced.".to_string(),
            "To clean them up, use a file manager.".to_string(),
            String::new(),
        ];
        lines.extend(Self::cut_short(duplicates));
        Self {
            title: "Duplicate games on the device".to_string(),
            lines,
            buttons: vec![DialogButton::new("Close", None)],
            focused_button: 0,
        }
    }

    pub(super) fn trash_games(entries: Vec<LocalEntry>) -> Self {
        let mut lines = vec![
            "These go to the system's trash, with their covers. Their saves stay.".to_string(),
            String::new(),
        ];
        lines.extend(Self::cut_short(
            entries
                .iter()
                .map(|local_entry| local_entry.name.clone())
                .collect(),
        ));
        Self::removal(
            format!("Trash {}", Self::pluralized(entries.len(), "game")),
            lines,
            "Trash",
            WorkerCommand::TrashGames(entries),
        )
    }

    pub(super) fn trash_orphans(orphans: Vec<OrphanLocalSave>) -> Self {
        let mut lines = vec![
            "Every version goes to the system's trash.".to_string(),
            String::new(),
        ];
        lines.extend(Self::cut_short(
            orphans.iter().map(ToString::to_string).collect(),
        ));
        Self::removal(
            format!("Trash {}", Self::pluralized(orphans.len(), "orphan save")),
            lines,
            "Trash",
            WorkerCommand::TrashOrphanSaves(orphans),
        )
    }

    pub(super) fn trash_version(local_entry: LocalEntry, version: LibrarySave) -> Self {
        Self::removal(
            "Trash a save".to_string(),
            vec![format!(
                "Version {} goes to the system's trash.",
                version.name
            )],
            "Trash",
            WorkerCommand::TrashSaveVersion(local_entry, version),
        )
    }

    /// `is_any_only_here` says whether a game among them isn't in the library.
    pub(super) fn delete_games(entries: Vec<DeviceEntry>, is_any_only_here: bool) -> Self {
        let names: Vec<String> = entries
            .iter()
            .map(|entry| entry.path.display().to_string())
            .collect();
        let is_single = entries.len() == 1;
        let (it, them) = if is_single {
            ("it", "It isn't")
        } else {
            ("them", "Some aren't")
        };
        let mut lines = vec![format!(
            "This deletes {it} from the device for good: there is no trash there."
        )];
        if is_any_only_here {
            lines.push(format!("{them} in the library."));
        }
        lines.push(if is_single {
            "Its save stays on the device.".to_string()
        } else {
            "Their saves stay on the device.".to_string()
        });
        lines.push(String::new());
        lines.extend(Self::cut_short(names));
        Self::removal(
            format!(
                "Delete {} from the device",
                Self::pluralized(entries.len(), "game")
            ),
            lines,
            "Delete",
            WorkerCommand::DeleteGames(entries),
        )
    }

    pub(super) fn delete_orphans(orphans: Vec<OrphanDeviceSave>) -> Self {
        let mut lines = vec![
            "This deletes them from the device for good: there is no trash there.".to_string(),
            String::new(),
        ];
        lines.extend(Self::cut_short(
            orphans.iter().map(ToString::to_string).collect(),
        ));
        Self::removal(
            format!(
                "Delete {} from the device",
                Self::pluralized(orphans.len(), "orphan save")
            ),
            lines,
            "Delete",
            WorkerCommand::DeleteOrphanSaves(orphans),
        )
    }

    /// `is_any_unsaved` says whether a save among them isn't in the library.
    pub(super) fn delete_save(entries: Vec<DeviceEntry>, is_any_unsaved: bool) -> Self {
        let mut lines = vec![
            "This deletes the save from the device for good: there is no trash there.".to_string(),
        ];
        if is_any_unsaved {
            lines.push("The library doesn't have this save.".to_string());
        }
        Self::removal(
            "Delete a save from the device".to_string(),
            lines,
            "Delete",
            WorkerCommand::DeleteSaves(entries),
        )
    }

    pub(super) fn remove_core(id: String) -> Self {
        Self::removal(
            format!("Delete {id} from the device"),
            vec![
                "This deletes the core for good: there is no trash there.".to_string(),
                "Its settings and saves stay on the device.".to_string(),
            ],
            "Delete",
            WorkerCommand::RemoveCores(vec![id]),
        )
    }

    /// A question whose answer removes something. It starts on Cancel.
    fn removal(
        title: String,
        lines: Vec<String>,
        label: &'static str,
        command: WorkerCommand,
    ) -> Self {
        Self {
            title,
            lines,
            buttons: vec![
                DialogButton::removal(label, command),
                DialogButton::new("Cancel", None),
            ],
            focused_button: 1,
        }
    }

    fn pluralized(count: usize, what: &str) -> String {
        match count {
            1 => format!("1 {what}"),
            count => format!("{count} {what}s"),
        }
    }

    /// Keeps the first 10 lines, and says how many more there are.
    fn cut_short(mut lines: Vec<String>) -> Vec<String> {
        const SHOWN: usize = 10;
        if lines.len() > SHOWN {
            let more = lines.len() - SHOWN;
            lines.truncate(SHOWN);
            lines.push(format!("… and {more} more"));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::games_page::testing::local_entry;

    #[test]
    fn renames_shows_five_games_and_counts_the_rest() {
        let renames = (0..7)
            .map(|i| {
                (
                    local_entry(&format!("{i}.gba"), i, &[]),
                    format!("Game {i}"),
                )
            })
            .collect();
        let dialog = Dialog::renames(renames);
        assert_eq!(dialog.title, "Rename 7 games");
        assert_eq!(dialog.lines[3..5], ["0", "  → Game 0"]);
        assert_eq!(dialog.lines.last().unwrap(), "… and 2 more");
    }
}
