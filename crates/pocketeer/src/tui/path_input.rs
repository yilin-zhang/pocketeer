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

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{Atom, AtomKind, CaseMatching, Normalization},
};
use tui_input::{Input, InputRequest, backend::crossterm::to_input_request};

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
        _ => to_input_request(&Event::Key(key)),
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

    /// Handles editing, moving through the matches, and Tab.
    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Down => self.pick_next(),
            KeyCode::Char('n') if ctrl => self.pick_next(),
            KeyCode::Up => self.pick_previous(),
            KeyCode::Char('p') if ctrl => self.pick_previous(),
            KeyCode::Tab => self.complete(),
            KeyCode::Char('w') if ctrl => self.cut_word(),
            _ => {
                if edit(&mut self.input, key) {
                    self.picked_match_index = None;
                }
            }
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

    fn pick_next(&mut self) {
        let last = self.matches().len().checked_sub(1);
        self.picked_match_index =
            last.map(|last| self.picked_match_index.map_or(0, |i| (i + 1).min(last)));
    }

    fn pick_previous(&mut self) {
        self.picked_match_index = self.picked_match_index.and_then(|i| i.checked_sub(1));
    }

    /// Completes to the picked or only match, or else as far as all matches
    /// starting with the text agree.
    fn complete(&mut self) {
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
    tx: Sender<PathBuf>,
    rx: Receiver<(PathBuf, Vec<PathEntry>)>,
}

impl DirReader {
    /// Starts the thread. `wake` is called when a folder has been read.
    pub fn spawn(wake: impl Fn() + Send + 'static) -> Self {
        let (tx, requests) = mpsc::channel::<PathBuf>();
        let (entries_tx, rx) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(first) = requests.recv() {
                // Only the latest request matters.
                let dir = requests.try_iter().last().unwrap_or(first);
                let entries = read_dir(&dir);
                if entries_tx.send((dir, entries)).is_err() {
                    return;
                }
                wake();
            }
        });
        Self { tx, rx }
    }

    pub fn request_dir(&self, dir: PathBuf) {
        let _ = self.tx.send(dir);
    }

    /// The folders read since the last time.
    pub fn dirs_read(&self) -> impl Iterator<Item = (PathBuf, Vec<PathEntry>)> + '_ {
        self.rx.try_iter()
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

    fn press(input: &mut PathInput, code: KeyCode) {
        input.handle_key(KeyEvent::from(code));
    }

    fn ctrl(input: &mut PathInput, character: char) {
        input.handle_key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::CONTROL,
        ));
    }

    fn text(input: &PathInput) -> String {
        let (before, after) = input.text_around_cursor();
        before + &after
    }

    fn names(input: &PathInput) -> Vec<&str> {
        input
            .matches()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect()
    }

    mod matches {
        use super::*;

        #[test]
        fn narrows_as_typed_and_lists_prefix_matches_first() {
            let entries = [
                entry("Amine.sav", false),
                entry("mine.sav", false),
                entry("other.sav", false),
            ];
            let mut input = input("/a/", &entries);
            assert_eq!(names(&input), ["Amine.sav", "mine.sav", "other.sav"]);
            for character in "MI".chars() {
                press(&mut input, KeyCode::Char(character));
            }
            assert_eq!(names(&input), ["mine.sav", "Amine.sav"]);
        }

        #[test]
        fn offers_hidden_entries_after_typing_dot() {
            let entries = [entry(".hidden", false), entry("shown", false)];
            let mut input = input("/a/", &entries);
            assert_eq!(names(&input), ["shown"]);
            press(&mut input, KeyCode::Char('.'));
            assert_eq!(names(&input), [".hidden"]);
        }

        #[test]
        fn offers_nothing_from_dir_text_has_left() {
            let mut input = input("/a/", &[entry("b", true)]);
            press(&mut input, KeyCode::Tab);
            assert_eq!(text(&input), "/a/b/");
            assert_eq!(input.matches(), Vec::<&PathEntry>::new());
            // Entries arriving late for a folder the text has left are ignored.
            input.set_entries(PathBuf::from("/a/"), vec![entry("late", false)]);
            assert_eq!(input.matches(), Vec::<&PathEntry>::new());
        }

        #[test]
        fn matches_letters_in_order() {
            let mut input = input("/a/", &[entry("Pokemon.gba", false), entry("other", false)]);
            for character in "pkmn".chars() {
                press(&mut input, KeyCode::Char(character));
            }
            assert_eq!(names(&input), ["Pokemon.gba"]);
        }

        #[test]
        fn returns_matched_character_positions() {
            let name = entry("Mine.sav", false);
            assert_eq!(name.matched_positions("in"), [1, 2]);
            assert_eq!(name.matched_positions("MINE"), [0, 1, 2, 3]);
            assert_eq!(name.matched_positions("zzz"), Vec::<usize>::new());
            assert_eq!(name.matched_positions(""), Vec::<usize>::new());
        }

        #[test]
        fn counts_positions_in_characters_not_bytes() {
            let name = entry("日本語 mine.sav", false);
            assert_eq!(name.matched_positions("mine"), [4, 5, 6, 7]);
        }

        /// A capital that lowercases to two characters, next to a name holding
        /// those two. It once split one in half and crashed the UI.
        #[test]
        fn keeps_positions_in_bounds_when_lowercase_is_longer() {
            let name = entry("i\u{0307}.gba", false);
            let chars = name.name.chars().count();
            assert!(
                name.matched_positions("\u{0130}")
                    .iter()
                    .all(|i| *i < chars)
            );
        }
    }

    mod picking {
        use super::*;

        #[test]
        fn tab_completes_only_match_with_slash_for_dir() {
            let mut only_file = input("/a/mi", &[entry("mine.sav", false)]);
            press(&mut only_file, KeyCode::Tab);
            assert_eq!(text(&only_file), "/a/mine.sav");
            let mut only_dir = input("/a/mi", &[entry("misc", true)]);
            press(&mut only_dir, KeyCode::Tab);
            assert_eq!(text(&only_dir), "/a/misc/");
        }

        #[test]
        fn tab_completes_common_prefix() {
            let mut input = input("/a/m", &[entry("mine-1", false), entry("mine-2", false)]);
            press(&mut input, KeyCode::Tab);
            assert_eq!(text(&input), "/a/mine-");
            // No further, because they differ from here.
            press(&mut input, KeyCode::Tab);
            assert_eq!(text(&input), "/a/mine-");
        }

        #[test]
        fn arrows_pick_match_and_tab_completes_it() {
            let mut input = input("/a/", &[entry("one", false), entry("two", false)]);
            press(&mut input, KeyCode::Down);
            press(&mut input, KeyCode::Down);
            press(&mut input, KeyCode::Down);
            assert_eq!(input.picked_match_index(), Some(1));
            press(&mut input, KeyCode::Tab);
            assert_eq!(text(&input), "/a/two");
            assert_eq!(input.picked_match_index(), None);
        }

        #[test]
        fn clears_pick_on_up_from_first_match_or_on_editing() {
            let mut input = input("/a/", &[entry("one", false)]);
            press(&mut input, KeyCode::Down);
            press(&mut input, KeyCode::Up);
            assert_eq!(input.picked_match_index(), None);
            press(&mut input, KeyCode::Down);
            press(&mut input, KeyCode::Char('o'));
            assert_eq!(input.picked_match_index(), None);
        }
    }

    mod editing {
        use super::*;

        #[test]
        fn edits_text_at_cursor() {
            let mut input = PathInput::new("/one/two");
            ctrl(&mut input, 'a');
            press(&mut input, KeyCode::Right);
            press(&mut input, KeyCode::Char('x'));
            assert_eq!(
                input.text_around_cursor(),
                ("/x".to_string(), "one/two".to_string())
            );
            ctrl(&mut input, 'k');
            assert_eq!(text(&input), "/x");
            ctrl(&mut input, 'e');
            press(&mut input, KeyCode::Backspace);
            assert_eq!(text(&input), "/");
        }

        #[test]
        fn cuts_word_back_to_slash() {
            let mut input = PathInput::new("/one/two/");
            ctrl(&mut input, 'w');
            assert_eq!(text(&input), "/one/");
            ctrl(&mut input, 'w');
            assert_eq!(text(&input), "/");
            ctrl(&mut input, 'u');
            assert_eq!(text(&input), "");
        }

        #[test]
        fn expands_tilde_to_home_dir() {
            let home = env::home_dir().unwrap();
            assert_eq!(PathInput::new("~/x/y").path(), home.join("x/y"));
            assert_eq!(PathInput::new("~").path(), home);
            assert_eq!(PathInput::new("~/x").dir(), home);
            // A `~` that is part of a longer name stays as it is.
            assert_eq!(PathInput::new("~x").path(), PathBuf::from("~x"));
        }
    }

    mod reading {
        use super::*;

        #[test]
        fn reads_dir_on_own_thread_sorted_by_name() {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("b.sav"), b"").unwrap();
            fs::write(dir.path().join("A.sav"), b"").unwrap();
            fs::create_dir(dir.path().join("c")).unwrap();
            let (woken, wake) = mpsc::channel();
            let reader = DirReader::spawn(move || {
                let _ = woken.send(());
            });
            reader.request_dir(dir.path().to_path_buf());
            wake.recv_timeout(Duration::from_secs(10)).unwrap();
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

        #[test]
        fn returns_nothing_for_unreadable_dir() {
            assert_eq!(read_dir(Path::new("/no/such/folder")), []);
        }
    }
}
