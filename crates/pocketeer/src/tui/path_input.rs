//! Typing text, and above all a path, with what is in its folder offered as
//! you go. The folder is
//! read on its own thread, so that a slow disk doesn't hold up the typing.

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
    /// It keeps a large buffer, so one is kept rather than one for each name.
    static MATCHER: RefCell<Matcher> = RefCell::new(Matcher::new(Config::DEFAULT));
}

/// How well `name` has the letters of `typed` in order, whatever their case,
/// and which of its characters they are; none if it doesn't. Every name has
/// nothing typed.
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
    let score = MATCHER.with_borrow_mut(|m| atom.indices(name, m, &mut found))?;
    found.sort_unstable();
    found.dedup();
    Some((score, found.into_iter().map(|i| i as usize).collect()))
}

/// Edits `input` as the Emacs keys and the others say; whether its text
/// changed. The keys `tui_input` doesn't have, or takes otherwise, are here.
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

/// The text before and after the cursor.
pub fn text_around_cursor(input: &Input) -> (String, String) {
    let text = input.value();
    let at = text.char_indices().nth(input.cursor());
    let (before, after) = text.split_at(at.map_or(text.len(), |(i, _)| i));
    (before.to_string(), after.to_string())
}

/// What a folder holds, as offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEntry {
    pub name: String,
    pub is_dir: bool,
}

impl PathEntry {
    /// Which of the name's characters `typed` matches, to show it.
    pub fn matched_positions(&self, typed: &str) -> Vec<usize> {
        fuzzy_match(typed, &self.name).map_or(Vec::new(), |(_, found)| found)
    }
}

/// The text of a path being typed, and what its folder offers.
pub struct PathInput {
    input: Input,
    /// The folder last asked to be read.
    asked: Option<PathBuf>,
    /// The folder `entries` are of.
    listed: Option<PathBuf>,
    /// By name.
    entries: Vec<PathEntry>,
    /// Which of the matches the arrow keys moved to.
    picked: Option<usize>,
}

impl PathInput {
    pub fn new(start: &str) -> Self {
        Self {
            input: Input::new(start.to_string()),
            asked: None,
            listed: None,
            entries: Vec::new(),
            picked: None,
        }
    }

    /// The text before and after the cursor.
    pub fn text_around_cursor(&self) -> (String, String) {
        text_around_cursor(&self.input)
    }

    /// The path typed, with `~` as the home folder.
    pub fn path(&self) -> PathBuf {
        expand_tilde(self.input.value())
    }

    /// What is typed after the last `/`.
    pub fn partial(&self) -> &str {
        &self.input.value()[self.partial_start()..]
    }

    /// Whether what is typed ends in `/`, so can only be a folder.
    pub fn is_dir(&self) -> bool {
        self.partial().is_empty() && !self.input.value().is_empty()
    }

    /// Whether what is typed is a folder, going by what has been read of the
    /// folder it is in.
    pub fn names_dir(&self) -> bool {
        let is_it = |e: &&PathEntry| e.is_dir && e.name == self.partial();
        self.is_dir() || self.matches().iter().any(is_it)
    }

    pub fn picked(&self) -> Option<usize> {
        self.picked
    }

    /// What the folder holds that is like what is typed, best first. Hidden
    /// ones only once a `.` is typed. None until the folder has been read.
    pub fn matches(&self) -> Vec<&PathEntry> {
        if self.listed.as_deref() != Some(self.dir().as_path()) {
            return Vec::new();
        }
        let partial = self.partial();
        let hidden = partial.starts_with('.');
        let mut found: Vec<(u16, &PathEntry)> = self
            .entries
            .iter()
            .filter(|e| hidden || !e.name.starts_with('.'))
            .filter_map(|e| Some((fuzzy_match(partial, &e.name)?.0, e)))
            .collect();
        found.sort_by_key(|(score, _)| Reverse(*score));
        found.into_iter().map(|(_, e)| e).collect()
    }

    /// The folder to read now, if it hasn't been asked for.
    pub fn dir_to_read(&mut self) -> Option<PathBuf> {
        let dir = self.dir();
        if self.asked.as_ref() == Some(&dir) {
            return None;
        }
        self.asked = Some(dir.clone());
        Some(dir)
    }

    /// Takes what a folder holds, unless the text has moved on from it.
    pub fn set_entries(&mut self, dir: PathBuf, entries: Vec<PathEntry>) {
        if dir == self.dir() {
            self.listed = Some(dir);
            self.entries = entries;
            self.picked = None;
        }
    }

    /// Lets go of the match picked; whether one was.
    pub fn unpick(&mut self) -> bool {
        self.picked.take().is_some()
    }

    /// Puts the match picked in the text; whether one was.
    pub fn fill_in_picked(&mut self) -> bool {
        let picked = self.picked.and_then(|i| self.matches().get(i).copied());
        let Some(entry) = picked.cloned() else {
            return false;
        };
        self.fill_in(&entry);
        true
    }

    /// Editing, moving through the matches, and Tab.
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
                    self.picked = None;
                }
            }
        }
    }

    /// Where what is typed after the last `/` starts.
    fn partial_start(&self) -> usize {
        self.input.value().rfind('/').map_or(0, |i| i + 1)
    }

    /// The folder of the path typed.
    fn dir(&self) -> PathBuf {
        match self.partial_start() {
            0 => PathBuf::from("."),
            n => expand_tilde(&self.input.value()[..n]),
        }
    }

    fn pick_next(&mut self) {
        let last = self.matches().len().checked_sub(1);
        self.picked = last.map(|last| self.picked.map_or(0, |i| (i + 1).min(last)));
    }

    fn pick_previous(&mut self) {
        self.picked = self.picked.and_then(|i| i.checked_sub(1));
    }

    /// The match picked or the only one; or else as far as all those starting
    /// with what is typed agree.
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
        let partial = self.partial().to_lowercase();
        let mut starting = matches
            .iter()
            .filter(|e| e.name.to_lowercase().starts_with(&partial))
            .map(|e| e.name.as_str());
        let Some(first) = starting.next() else {
            return;
        };
        let common = starting.fold(first, |common, name| {
            let same = common.chars().zip(name.chars()).take_while(|(a, b)| a == b);
            let bytes = same.map(|(c, _)| c.len_utf8()).sum();
            &common[..bytes]
        });
        if common.chars().count() > partial.chars().count() {
            self.set_partial(common.to_string());
        }
    }

    fn fill_in(&mut self, entry: &PathEntry) {
        let slash = if entry.is_dir { "/" } else { "" };
        self.set_partial(format!("{}{slash}", entry.name));
    }

    fn set_partial(&mut self, partial: String) {
        let dir = &self.input.value()[..self.partial_start()];
        self.input = Input::new(format!("{dir}{partial}"));
        self.picked = None;
    }

    /// Cuts the word before the cursor: a word ends at `/` or a space.
    fn cut_word(&mut self) {
        let is_end = |c: char| c == '/' || c == ' ';
        let (before, after) = self.text_around_cursor();
        let word = before.trim_end_matches(is_end);
        let kept = &word[..word.rfind(is_end).map_or(0, |i| i + 1)];
        self.input = Input::new(format!("{kept}{after}")).with_cursor(kept.chars().count());
        self.picked = None;
    }
}

/// Reads folders on its own thread.
pub struct DirReader {
    tx: Sender<PathBuf>,
    rx: Receiver<(PathBuf, Vec<PathEntry>)>,
}

impl DirReader {
    /// Starts the thread; `wake` is called when a folder has been read.
    pub fn spawn(wake: impl Fn() + Send + 'static) -> Self {
        let (tx, requests) = mpsc::channel::<PathBuf>();
        let (done, rx) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(first) = requests.recv() {
                // Only the last one asked for matters.
                let dir = requests.try_iter().last().unwrap_or(first);
                let entries = read_dir(&dir);
                if done.send((dir, entries)).is_err() {
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

/// What `dir` holds, by name; nothing if it can't be read.
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
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries
}

/// `path` with `~` as the home folder, if there is one.
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

    fn ctrl(input: &mut PathInput, c: char) {
        input.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    fn text(input: &PathInput) -> String {
        let (before, after) = input.text_around_cursor();
        before + &after
    }

    fn names(input: &PathInput) -> Vec<&str> {
        input.matches().iter().map(|e| e.name.as_str()).collect()
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
            for c in "MI".chars() {
                press(&mut input, KeyCode::Char(c));
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
            // What comes late for the folder it has left is not taken.
            input.set_entries(PathBuf::from("/a/"), vec![entry("late", false)]);
            assert_eq!(input.matches(), Vec::<&PathEntry>::new());
        }

        #[test]
        fn matches_letters_in_order() {
            let mut input = input("/a/", &[entry("Pokemon.gba", false), entry("other", false)]);
            for c in "pkmn".chars() {
                press(&mut input, KeyCode::Char(c));
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

        /// A capital that lowercases to two characters, by a name holding those
        /// two: it once cut one in half, and took the whole UI down.
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
            // No further: they differ from here.
            press(&mut input, KeyCode::Tab);
            assert_eq!(text(&input), "/a/mine-");
        }

        #[test]
        fn arrows_pick_match_and_tab_completes_it() {
            let mut input = input("/a/", &[entry("one", false), entry("two", false)]);
            press(&mut input, KeyCode::Down);
            press(&mut input, KeyCode::Down);
            press(&mut input, KeyCode::Down);
            assert_eq!(input.picked(), Some(1));
            press(&mut input, KeyCode::Tab);
            assert_eq!(text(&input), "/a/two");
            assert_eq!(input.picked(), None);
        }

        #[test]
        fn clears_pick_on_up_from_first_match_or_on_editing() {
            let mut input = input("/a/", &[entry("one", false)]);
            press(&mut input, KeyCode::Down);
            press(&mut input, KeyCode::Up);
            assert_eq!(input.picked(), None);
            press(&mut input, KeyCode::Down);
            press(&mut input, KeyCode::Char('o'));
            assert_eq!(input.picked(), None);
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
            // Only as a whole name.
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
