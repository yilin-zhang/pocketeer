//! Text input, especially for paths, which offers what is in the folder while
//! typing. The folder is read on its own thread so that a slow disk doesn't
//! hold up typing.

use std::{
    cell::RefCell,
    cmp::Reverse,
    env, fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{Atom, AtomKind, CaseMatching, Normalization},
};
use tui_input::{Input, InputRequest, backend::crossterm::to_input_request};

use super::Event;

thread_local! {
    /// It keeps a large buffer, so one is shared instead of creating one per
    /// name.
    static MATCHER: RefCell<Matcher> = RefCell::new(Matcher::new(Config::DEFAULT));
}

/// How well `name` contains the letters of `typed` in order, ignoring case, and
/// which of its characters they are. Returns none if it doesn't match. Empty
/// `typed` matches every name.
pub fn fuzzy_match(typed: &str, name: &str) -> Option<(u16, Vec<usize>)> {
    if typed.is_empty() {
        return Some((0, Vec::new()));
    }
    let atom = Atom::new(
        typed,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );
    let mut chars = Vec::new();
    let name = Utf32Str::new(name, &mut chars);
    let mut found = Vec::new();
    let score = MATCHER.with_borrow_mut(|matcher| atom.indices(name, matcher, &mut found))?;
    found.sort_unstable();
    found.dedup();
    Some((score, found.into_iter().map(|i| i as usize).collect()))
}

/// Applies `key` to `input`, including the Emacs keys `tui_input` lacks or
/// handles differently. Returns whether the text changed.
pub fn edit(input: &mut Input, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let request = match key.code {
        KeyCode::Char('u') if ctrl => Some(InputRequest::DeleteFromStart),
        KeyCode::Char('d') if ctrl => Some(InputRequest::DeleteNextChar),
        _ => to_input_request(&TerminalEvent::Key(key)),
    };
    let changed = request.and_then(|request| input.handle(request));
    changed.is_some_and(|changed| changed.value)
}

pub fn text_around_cursor(input: &Input) -> (String, String) {
    let text = input.value();
    let at = text.char_indices().nth(input.cursor());
    let (before, after) = text.split_at(at.map_or(text.len(), |(i, _)| i));
    (before.to_string(), after.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEntry {
    pub name: String,
    pub is_dir: bool,
}

impl PathEntry {
    /// Which of the name's characters match `typed`, for highlighting.
    pub fn matched_positions(&self, typed: &str) -> Vec<usize> {
        fuzzy_match(typed, &self.name).map_or(Vec::new(), |(_, found)| found)
    }
}

/// The text of a path being typed, and what its folder offers.
pub struct PathInput {
    input: Input,
    /// The folder last requested.
    requested_dir: Option<PathBuf>,
    /// The folder `entries` came from.
    entries_dir: Option<PathBuf>,
    /// Sorted by name.
    entries: Vec<PathEntry>,
    /// Which of the matches the arrow keys moved to.
    picked_match_index: Option<usize>,
}

impl PathInput {
    pub fn new(start: &str) -> Self {
        Self {
            input: Input::new(start.to_string()),
            requested_dir: None,
            entries_dir: None,
            entries: Vec::new(),
            picked_match_index: None,
        }
    }

    pub fn text_around_cursor(&self) -> (String, String) {
        text_around_cursor(&self.input)
    }

    /// The path typed, with `~` as the home folder.
    pub fn path(&self) -> PathBuf {
        expand_tilde(self.input.value())
    }

    /// What is typed after the last `/`.
    pub fn typed_name(&self) -> &str {
        &self.input.value()[self.typed_name_start()..]
    }

    /// Whether the text ends in `/`, which means it can only be a folder.
    pub fn ends_with_slash(&self) -> bool {
        self.typed_name().is_empty() && !self.input.value().is_empty()
    }

    /// Whether the text names a folder, judging by what has been read of its
    /// parent.
    pub fn names_dir(&self) -> bool {
        let is_named_dir = |entry: &&PathEntry| entry.is_dir && entry.name == self.typed_name();
        self.ends_with_slash() || self.matches().iter().any(is_named_dir)
    }

    pub fn picked_match_index(&self) -> Option<usize> {
        self.picked_match_index
    }

    /// The entries matching the text, best first. Hidden ones only appear once
    /// a `.` is typed. None until the folder has been read.
    pub fn matches(&self) -> Vec<&PathEntry> {
        if self.entries_dir.as_deref() != Some(self.dir().as_path()) {
            return Vec::new();
        }
        let typed_name = self.typed_name();
        let hidden = typed_name.starts_with('.');
        let mut found: Vec<(u16, &PathEntry)> = self
            .entries
            .iter()
            .filter(|entry| hidden || !entry.name.starts_with('.'))
            .filter_map(|entry| Some((fuzzy_match(typed_name, &entry.name)?.0, entry)))
            .collect();
        found.sort_by_key(|(score, _)| Reverse(*score));
        found.into_iter().map(|(_, entry)| entry).collect()
    }

    /// The folder to read now, unless it has already been requested.
    pub fn dir_to_read(&mut self) -> Option<PathBuf> {
        let dir = self.dir();
        if self.requested_dir.as_ref() == Some(&dir) {
            return None;
        }
        self.requested_dir = Some(dir.clone());
        Some(dir)
    }

    /// Stores what a folder holds, unless the text has moved on from it.
    pub fn set_entries(&mut self, dir: PathBuf, entries: Vec<PathEntry>) {
        if dir == self.dir() {
            self.entries_dir = Some(dir);
            self.entries = entries;
            self.picked_match_index = None;
        }
    }

    /// Clears the picked match and returns whether there was one.
    pub fn unpick(&mut self) -> bool {
        self.picked_match_index.take().is_some()
    }

    /// Puts the picked match in the text and returns whether there was one.
    pub fn fill_in_picked(&mut self) -> bool {
        let picked = self
            .picked_match_index
            .and_then(|i| self.matches().get(i).copied());
        let Some(entry) = picked.cloned() else {
            return false;
        };
        self.fill_in(&entry);
        true
    }

    /// Edits the text with `key`, including C-w.
    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('w') {
            self.cut_word();
        } else if edit(&mut self.input, key) {
            self.picked_match_index = None;
        }
    }

    /// Where what is typed after the last `/` starts.
    fn typed_name_start(&self) -> usize {
        self.input.value().rfind('/').map_or(0, |i| i + 1)
    }

    fn dir(&self) -> PathBuf {
        match self.typed_name_start() {
            0 => PathBuf::from("."),
            slash_index => expand_tilde(&self.input.value()[..slash_index]),
        }
    }

    pub fn pick_next(&mut self) {
        let last = self.matches().len().checked_sub(1);
        self.picked_match_index =
            last.map(|last| self.picked_match_index.map_or(0, |i| (i + 1).min(last)));
    }

    pub fn pick_previous(&mut self) {
        self.picked_match_index = self.picked_match_index.and_then(|i| i.checked_sub(1));
    }

    /// Completes to the picked or only match, or else as far as all matches
    /// starting with the text agree.
    pub fn complete(&mut self) {
        if self.fill_in_picked() {
            return;
        }
        let matches = self.matches();
        if let [only] = matches[..] {
            let only = only.clone();
            self.fill_in(&only);
            return;
        }
        let typed_name = self.typed_name().to_lowercase();
        let mut starting = matches
            .iter()
            .filter(|entry| entry.name.to_lowercase().starts_with(&typed_name))
            .map(|entry| entry.name.as_str());
        let Some(first) = starting.next() else {
            return;
        };
        let common = starting.fold(first, |common, name| {
            let same = common.chars().zip(name.chars()).take_while(|(a, b)| a == b);
            let bytes = same.map(|(character, _)| character.len_utf8()).sum();
            &common[..bytes]
        });
        if common.chars().count() > typed_name.chars().count() {
            self.set_typed_name(common.to_string());
        }
    }

    fn fill_in(&mut self, entry: &PathEntry) {
        let slash = if entry.is_dir { "/" } else { "" };
        self.set_typed_name(format!("{}{slash}", entry.name));
    }

    fn set_typed_name(&mut self, typed_name: String) {
        let dir = &self.input.value()[..self.typed_name_start()];
        self.input = Input::new(format!("{dir}{typed_name}"));
        self.picked_match_index = None;
    }

    /// Cuts the word before the cursor. A word ends at `/` or a space.
    fn cut_word(&mut self) {
        let is_end = |character: char| character == '/' || character == ' ';
        let (before, after) = self.text_around_cursor();
        let word = before.trim_end_matches(is_end);
        let kept = &word[..word.rfind(is_end).map_or(0, |i| i + 1)];
        self.input = Input::new(format!("{kept}{after}")).with_cursor(kept.chars().count());
        self.picked_match_index = None;
    }
}

/// Reads folders on its own thread.
pub struct DirReader {
    request_tx: Sender<PathBuf>,
    entries_rx: Receiver<(PathBuf, Vec<PathEntry>)>,
}

impl DirReader {
    /// Starts the thread. `events` hears when a folder has been read.
    pub fn spawn(events: Sender<Event>) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<PathBuf>();
        let (entries_tx, entries_rx) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(first) = request_rx.recv() {
                // Only the latest request matters.
                let dir = request_rx.try_iter().last().unwrap_or(first);
                let entries = read_dir(&dir);
                if entries_tx.send((dir, entries)).is_err() {
                    return;
                }
                let _ = events.send(Event::DirRead);
            }
        });
        Self {
            request_tx,
            entries_rx,
        }
    }

    pub fn request_dir(&self, dir: PathBuf) {
        let _ = self.request_tx.send(dir);
    }

    /// The folders read since the last time.
    pub fn dirs_read(&self) -> impl Iterator<Item = (PathBuf, Vec<PathEntry>)> + '_ {
        self.entries_rx.try_iter()
    }
}

/// What `dir` holds, by name, or nothing if it can't be read.
fn read_dir(dir: &Path) -> Vec<PathEntry> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<PathEntry> = read
        .filter_map(Result::ok)
        .map(|entry| PathEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: entry.path().is_dir(),
        })
        .collect();
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    entries
}

/// `path` with `~` expanded to the home folder, if there is one.
fn expand_tilde(path: &str) -> PathBuf {
    let rest = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest.trim_start_matches('/'),
        _ => return PathBuf::from(path),
    };
    match env::home_dir() {
        Some(home) if rest.is_empty() => home,
        Some(home) => home.join(rest),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn entry(name: &str, is_dir: bool) -> PathEntry {
        PathEntry {
            name: name.to_string(),
            is_dir,
        }
    }

    /// `text` in a folder that holds `entries`.
    fn input(text: &str, entries: &[PathEntry]) -> PathInput {
        let mut input = PathInput::new(text);
        input.set_entries(input.dir(), entries.to_vec());
        input
    }

    #[test]
    fn names_dir_when_text_ends_with_slash_or_matches_dir() {
        let entries = [entry("roms", true), entry("Mine.gba", false)];
        assert!(input("/some/", &entries).names_dir());
        assert!(input("/some/roms", &entries).names_dir());
        assert!(!input("/some/Mine.gba", &entries).names_dir());
        assert!(!input("/some/rom", &entries).names_dir());
    }

    fn text(input: &PathInput) -> String {
        let (before, after) = input.text_around_cursor();
        before + &after
    }

    mod matches {
        use super::*;

        #[test]
        fn offers_nothing_from_dir_text_has_left() {
            let mut input = input("/a/", &[entry("b", true)]);
            input.complete();
            assert_eq!(text(&input), "/a/b/");
            assert_eq!(input.matches(), Vec::<&PathEntry>::new());
            // Entries arriving late for a folder the text has left are ignored.
            input.set_entries(PathBuf::from("/a/"), vec![entry("late", false)]);
            assert_eq!(input.matches(), Vec::<&PathEntry>::new());
        }

        /// The positions to highlight count characters, not bytes, and stay
        /// within the name.
        #[test]
        fn matched_positions_are_character_indices_in_bounds() {
            let name = entry("Mine.sav", false);
            assert_eq!(name.matched_positions("in"), [1, 2]);
            assert_eq!(name.matched_positions("MINE"), [0, 1, 2, 3]);
            assert_eq!(name.matched_positions("zzz"), Vec::<usize>::new());
            assert_eq!(name.matched_positions(""), Vec::<usize>::new());
            let name = entry("日本語 mine.sav", false);
            assert_eq!(name.matched_positions("mine"), [4, 5, 6, 7]);
            // A capital that lowercases to two characters, next to a name
            // holding those two. It once split one in half and crashed the UI.
            let name = entry("i\u{0307}.gba", false);
            let chars = name.name.chars().count();
            assert!(
                name.matched_positions("\u{0130}")
                    .iter()
                    .all(|i| *i < chars)
            );
        }
    }

    #[test]
    fn reads_dir_on_own_thread_sorted_by_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b.sav"), b"").unwrap();
        fs::write(dir.path().join("A.sav"), b"").unwrap();
        fs::create_dir(dir.path().join("c")).unwrap();
        let (events, woken) = mpsc::channel();
        let reader = DirReader::spawn(events);
        reader.request_dir(dir.path().to_path_buf());
        woken.recv_timeout(Duration::from_secs(10)).unwrap();
        let (read, entries) = reader.dirs_read().next().unwrap();
        assert_eq!(read, dir.path());
        assert_eq!(
            entries,
            [
                entry("A.sav", false),
                entry("b.sav", false),
                entry("c", true)
            ]
        );
    }
}
