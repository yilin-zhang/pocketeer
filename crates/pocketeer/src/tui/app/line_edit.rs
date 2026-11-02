//! A one-line text field with Emacs keys, for prompts and list filters.
//!
//! C-a / C-e start and end, C-b / C-f a character, M-b / M-f a word (Emacs
//! style: M-f stops at the end of the word), C-d / C-h and Delete /
//! Backspace a character, M-d / M-Backspace / C-w a word, C-k to the end,
//! C-u to the start, C-y puts back what was cut last. Arrow keys, Home and
//! End work too. Alt arrives as `ALT` from terminals (Option as Meta), so
//! both `ALT` and `META` count.
//!
//! The cursor is a char index; a word is a run of letters and digits, so a
//! path's `/`, `.`, ` ` and `-` all end one.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineEdit {
    text: String,
    /// In chars, 0..=len.
    cursor: usize,
    /// The last text C-k / C-u / C-w / M-d cut, for C-y.
    cut: String,
}

impl LineEdit {
    /// `text` with the cursor at its end.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        LineEdit {
            text,
            cursor,
            cut: String::new(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Replaces the text and puts the cursor at its end.
    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.len();
    }

    pub fn clear(&mut self) {
        self.set(String::new());
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The text before and after the cursor.
    pub fn split(&self) -> (&str, &str) {
        self.text.split_at(self.byte(self.cursor))
    }

    /// Applies an editing key. `None` when the key isn't one (the caller
    /// handles it); otherwise whether the text changed.
    pub fn key(&mut self, key: KeyEvent) -> Option<bool> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::META);
        let before = self.text.clone();
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Home => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.len(),
            KeyCode::End => self.cursor = self.len(),
            KeyCode::Char('b') if ctrl => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Left if ctrl || alt => self.cursor = self.word_back(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('f') if ctrl => self.cursor = (self.cursor + 1).min(self.len()),
            KeyCode::Right if ctrl || alt => self.cursor = self.word_forward(),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.len()),
            KeyCode::Char('b') if alt => self.cursor = self.word_back(),
            KeyCode::Char('f') if alt => self.cursor = self.word_forward(),
            KeyCode::Backspace if alt || ctrl => self.cut_to(self.word_back()),
            KeyCode::Char('w') if ctrl => self.cut_to(self.word_back()),
            KeyCode::Char('d') if alt => self.cut_to(self.word_forward()),
            KeyCode::Char('k') if ctrl => self.cut_to(self.len()),
            KeyCode::Char('u') if ctrl => self.cut_to(0),
            KeyCode::Backspace => self.remove(self.cursor.saturating_sub(1), self.cursor),
            KeyCode::Char('h') if ctrl => self.remove(self.cursor.saturating_sub(1), self.cursor),
            KeyCode::Delete => self.remove(self.cursor, (self.cursor + 1).min(self.len())),
            KeyCode::Char('d') if ctrl => {
                self.remove(self.cursor, (self.cursor + 1).min(self.len()))
            }
            KeyCode::Char('y') if ctrl => {
                let at = self.byte(self.cursor);
                let cut = self.cut.clone();
                self.text.insert_str(at, &cut);
                self.cursor += cut.chars().count();
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                let at = self.byte(self.cursor);
                self.text.insert(at, c);
                self.cursor += 1;
            }
            _ => return None,
        }
        Some(self.text != before)
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(i, _)| i)
    }

    fn chars(&self) -> Vec<char> {
        self.text.chars().collect()
    }

    /// Emacs M-b: back over non-word chars, then over the word.
    fn word_back(&self) -> usize {
        let chars = self.chars();
        let mut i = self.cursor;
        while i > 0 && !chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    /// Emacs M-f: over non-word chars, then to the end of the word.
    fn word_forward(&self) -> usize {
        let chars = self.chars();
        let mut i = self.cursor;
        while i < chars.len() && !chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    /// Removes the chars between the cursor and `to` (either side).
    fn remove(&mut self, from: usize, to: usize) {
        let (a, b) = (from.min(to), from.max(to));
        let (start, end) = (self.byte(a), self.byte(b));
        self.text.replace_range(start..end, "");
        self.cursor = a;
    }

    /// Like `remove` from the cursor, keeping the text for C-y.
    fn cut_to(&mut self, to: usize) {
        let (a, b) = (self.cursor.min(to), self.cursor.max(to));
        if a == b {
            return;
        }
        self.cut = self.text[self.byte(a)..self.byte(b)].to_string();
        self.remove(a, b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(e: &mut LineEdit, code: KeyCode, modifiers: KeyModifiers) -> Option<bool> {
        e.key(KeyEvent::new(code, modifiers))
    }
    fn ctrl(e: &mut LineEdit, c: char) {
        press(e, KeyCode::Char(c), KeyModifiers::CONTROL);
    }
    fn alt(e: &mut LineEdit, c: char) {
        press(e, KeyCode::Char(c), KeyModifiers::ALT);
    }

    #[test]
    fn emacs_motion_and_kills_on_a_path() {
        let mut e = LineEdit::new("~/Downloads/gba roms/");
        alt(&mut e, 'b');
        assert_eq!(e.split().1, "roms/");
        alt(&mut e, 'b');
        assert_eq!(e.split().1, "gba roms/");
        alt(&mut e, 'f');
        // Emacs M-f stops at the end of the word.
        assert_eq!(e.split(), ("~/Downloads/gba", " roms/"));
        ctrl(&mut e, 'b');
        ctrl(&mut e, 'b');
        assert_eq!(e.split().1, "ba roms/");
        ctrl(&mut e, 'f');
        assert_eq!(e.split().1, "a roms/");
        ctrl(&mut e, 'a');
        assert_eq!(e.split().0, "");
        ctrl(&mut e, 'e');
        assert_eq!(e.split().1, "");

        ctrl(&mut e, 'w');
        assert_eq!(e.as_str(), "~/Downloads/gba ");
        ctrl(&mut e, 'y');
        assert_eq!(e.as_str(), "~/Downloads/gba roms/");
        ctrl(&mut e, 'a');
        alt(&mut e, 'd');
        // Like Emacs kill-word: the punctuation before the word goes too.
        assert_eq!(e.as_str(), "/gba roms/");
        alt(&mut e, 'f');
        ctrl(&mut e, 'k');
        assert_eq!(e.as_str(), "/gba");
        ctrl(&mut e, 'u');
        assert!(e.is_empty());
    }

    #[test]
    fn typing_and_deleting_in_the_middle_with_wide_chars() {
        let mut e = LineEdit::new("目录b");
        ctrl(&mut e, 'b');
        assert_eq!(
            press(&mut e, KeyCode::Char('a'), KeyModifiers::NONE),
            Some(true)
        );
        assert_eq!(e.as_str(), "目录ab");
        assert_eq!(
            press(&mut e, KeyCode::Backspace, KeyModifiers::NONE),
            Some(true)
        );
        ctrl(&mut e, 'b');
        ctrl(&mut e, 'd');
        assert_eq!(e.as_str(), "目b");
        assert_eq!(
            press(&mut e, KeyCode::Left, KeyModifiers::NONE),
            Some(false)
        );
        // Not an editing key: left to the caller.
        assert_eq!(press(&mut e, KeyCode::Enter, KeyModifiers::NONE), None);
        assert_eq!(
            press(&mut e, KeyCode::Char('n'), KeyModifiers::CONTROL),
            None
        );
    }
}
