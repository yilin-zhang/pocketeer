//! The look of the TUI: the palette and the small styles built from it.
//! Drawing code takes colours and styled pieces from here and never picks
//! a colour of its own.

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
    widgets::{Block, BorderType, Cell},
};

/// The palette. Analogue OS is black and white, so the UI is greys; colour
/// is kept for state so it means something when it shows up. Indexed
/// colours render the same in every 256-colour terminal. Drawing code
/// names a role from here and never picks a colour of its own. (Platforms
/// are an open set, so their colours are a table in `platform_cell`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Palette {
    // Greys, from text to background.
    Ink,
    Muted,
    Faint,
    Keycap,
    Rule,
    Paper,
    // State.
    Good,
    Warn,
    Bad,
    // Keycap tints by what a key does: muted, at one lightness, so a
    // legend reads as one bar with a few soft blocks.
    KeyLibrary,
    KeyCard,
    KeyEdit,
    KeyRemove,
}

impl Palette {
    pub(super) const fn color(self) -> Color {
        Color::Indexed(match self {
            Palette::Ink => 255,
            Palette::Muted => 246,
            Palette::Faint => 240,
            Palette::Keycap => 238,
            Palette::Rule => 237,
            Palette::Paper => 232,
            Palette::Good => 114,
            Palette::Warn => 179,
            Palette::Bad => 167,
            Palette::KeyLibrary => 65,
            Palette::KeyCard => 60,
            Palette::KeyEdit => 96,
            Palette::KeyRemove => 95,
        })
    }

    /// Text in this colour.
    pub(super) const fn style(self) -> Style {
        Style::new().fg(self.color())
    }

    /// `text` in this colour.
    pub(super) fn span(self, text: impl Into<String>) -> Span<'static> {
        Span::styled(text.into(), self.style())
    }
}

/// A platform as the tables show it: its id in capitals, in a colour of
/// its own so platforms tell apart at a glance. The colours are soft, so
/// they don't read as the green / amber / red that mean a state.
pub(super) fn platform_cell(id: &str) -> Cell<'static> {
    const KNOWN: &[(&str, u8)] = &[
        ("gb", 143),
        ("gbc", 175),
        ("gba", 140),
        ("nes", 174),
        ("snes", 104),
        ("genesis", 67),
        ("sms", 109),
        ("gg", 73),
        ("pce", 180),
        ("lynx", 137),
        ("gbstudio", 151),
    ];
    const OTHERS: [u8; 6] = [110, 146, 182, 144, 116, 181];
    let color = KNOWN
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, c)| *c)
        .unwrap_or_else(|| {
            let sum: usize = id.bytes().map(usize::from).sum();
            OTHERS[sum % OTHERS.len()]
        });
    let label = match id {
        "gbstudio" => "GB STUDIO".to_string(),
        _ => id.to_uppercase(),
    };
    Cell::from(label).style(Style::new().fg(Color::Indexed(color)))
}

/// The selected table row, the active filter and the focused dialog button:
/// one solid white bar, like a hardware menu. It overrides every cell's own
/// colour so the row reads as one line.
pub(super) fn selected() -> Style {
    Style::new()
        .bg(Palette::Ink.color())
        .fg(Palette::Paper.color())
        .add_modifier(Modifier::BOLD)
        .remove_modifier(Modifier::DIM)
}

/// A panel: rounded hairline border, small muted title.
pub(super) fn panel(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Palette::Rule.style())
        .title(Span::styled(
            format!(" {} ", title.into()),
            Palette::Muted.style(),
        ))
}

/// A key in a legend, tinted by what it does: sage brings something into
/// the library, slate writes to the card, mauve renames or changes covers,
/// rose removes; the rest are grey. A letter means the same thing on every
/// tab; lowercase acts on the selected row, uppercase on all of them.
pub(super) fn keycap(key: &str) -> Span<'static> {
    let tint = match key {
        "b" | "B" | "a" | "i" | "I" | "n" => Palette::KeyLibrary,
        "s" | "p" | "u" | "U" => Palette::KeyCard,
        "e" | "t" | "d" | "c" | "f" => Palette::KeyEdit,
        "x" => Palette::KeyRemove,
        _ => Palette::Keycap,
    };
    Span::styled(
        format!(" {key} "),
        Palette::Ink
            .style()
            .bg(tint.color())
            .add_modifier(Modifier::BOLD),
    )
}
