//! Importing ROMs from this computer, as a flow of prompts: the path, then
//! (only if the files don't say) the platform, then (only if some files
//! look like known games) which standard names to take, then the plan.

use std::{collections::HashSet, path::PathBuf};

use super::{
    Cx,
    dialog::{Answer, Step as DialogStep},
    path_input::{expand_home, start_path},
};
use crate::tui::{
    pending::{Import, Request},
    worker::Job,
};

/// The last item of the platform list, for typing another id.
const OTHER_PLATFORM: &str = "another platform id…";

/// Where an import is.
pub enum Step {
    /// What to import: a folder, a ROM or a zip.
    Source,
    /// A platform picked for files that don't say; the last item types one.
    PickPlatform {
        dir: PathBuf,
        platforms: Vec<String>,
    },
    /// A platform typed in.
    TypePlatform { dir: PathBuf },
    /// Standard names offered for the planned import.
    Names {
        import: Box<Import>,
        /// (platform, current name) per item, in item order.
        keys: Vec<(String, String)>,
    },
}

/// `a` on the Games tab.
pub fn start(cx: &mut Cx) {
    ask_source(cx, start_path());
}

fn ask_source(cx: &mut Cx, value: String) {
    if cx.session.require_library().is_none() {
        return;
    }
    cx.dialog.input(
        "Import ROMs",
        vec!["A folder, a ROM or a zip.".into()],
        value,
        DialogStep::Import(Step::Source),
    );
}

pub fn answer(cx: &mut Cx, step: Step, answer: Answer) {
    match (step, answer) {
        (Step::Source, Answer::Text(value)) => {
            let source = PathBuf::from(expand_home(value.trim()));
            if !source.is_dir() && !source.is_file() {
                cx.session
                    .notice(format!("{} is not a folder or file.", source.display()));
                ask_source(cx, value);
                return;
            }
            // A folder named after a platform takes the files whose
            // extension doesn't say.
            let known = cx.session.known_platforms();
            let platform = source
                .is_dir()
                .then(|| source.file_name())
                .flatten()
                .map(|n| n.to_string_lossy().to_lowercase())
                .filter(|n| known.contains(n));
            plan(cx, source, platform);
        }
        (Step::PickPlatform { dir, platforms }, Answer::Choice(i)) => match platforms.get(i) {
            Some(p) => plan(cx, dir, Some(p.clone())),
            None => type_platform(cx, dir),
        },
        (Step::TypePlatform { dir }, Answer::Text(value)) => {
            let platform = value.trim().to_lowercase();
            if platform.is_empty() || platform.contains(['/', ' ']) {
                cx.session
                    .notice("A platform id looks like gba or snes: letters and digits only.");
                type_platform(cx, dir);
                return;
            }
            plan(cx, dir, Some(platform));
        }
        (Step::Names { import, keys }, Answer::Checked(on)) => {
            let accepted: HashSet<(String, String)> = keys
                .into_iter()
                .zip(on)
                .filter(|(_, on)| *on)
                .map(|(k, _)| k)
                .collect();
            cx.session.submit(
                "Applying standard names",
                Job::Plan(Request::AcceptNames { import, accepted }),
            );
        }
        _ => {}
    }
}

fn plan(cx: &mut Cx, source: PathBuf, platform: Option<String>) {
    cx.session.submit(
        "Reading the folder",
        Job::Plan(Request::ImportFolder { source, platform }),
    );
}

/// The planned files don't say their platform: pick one.
pub fn ask_platform(cx: &mut Cx, dir: PathBuf, files: usize) {
    let platforms = cx.session.known_platforms();
    if platforms.is_empty() {
        type_platform(cx, dir);
        return;
    }
    let mut items = platforms.clone();
    items.push(OTHER_PLATFORM.into());
    cx.dialog.pick(
        format!("Which platform are these {files} file(s) for?"),
        items,
        0,
        DialogStep::Import(Step::PickPlatform { dir, platforms }),
    );
}

fn type_platform(cx: &mut Cx, dir: PathBuf) {
    cx.dialog.input(
        "Platform id",
        vec!["For example gba, gb or snes.".into()],
        String::new(),
        DialogStep::Import(Step::TypePlatform { dir }),
    );
}

/// The planned import has files that look like known games: ask which
/// standard names to take.
pub fn offer_names(cx: &mut Cx, import: Box<Import>) {
    let suggestions = import.suggestions();
    let items = suggestions
        .iter()
        .map(|(_, old, new)| format!("{old}  →  {new}"))
        .collect();
    let keys = suggestions
        .into_iter()
        .map(|(platform, old, _)| (platform, old))
        .collect();
    cx.dialog.toggle(
        "These look like known games. Import under the standard name?",
        items,
        DialogStep::Import(Step::Names { import, keys }),
    );
}
