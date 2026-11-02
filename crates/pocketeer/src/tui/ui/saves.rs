//! Saves on the Games tab (the SAVE column, the summary line, the saves
//! panel) and the Memories tab.

use super::*;
use pocketeer_core::transfer::saves::SaveAction;

use crate::{
    ops::memories::MemoryEntry,
    text::memories::{memory_game, memory_where},
    tui::app::{GameRef, SaveLine, SaveMark, memory_game as library_game, save_status},
};

/// The SAVE column of a Games row.
pub(super) fn save_cell(app: &App, row: GameRef) -> Cell<'static> {
    match app.games().save_mark(row, app.session()) {
        Some(SaveMark::Conflict) => Cell::from("!").style(Palette::Warn.style()),
        Some(SaveMark::CardNewer) => Cell::from("←").style(Palette::Warn.style()),
        Some(SaveMark::BackedUp(0)) => Cell::from("✓").style(Palette::Good.style()),
        Some(SaveMark::BackedUp(n)) => Cell::from(format!("✓ {n}")).style(Palette::Good.style()),
        Some(SaveMark::LibraryOnly) => Cell::from("·").style(Palette::Muted.style()),
        None => Cell::from(""),
    }
}

/// The details line about a game's saves, if it has any.
pub(super) fn save_summary(app: &App, row: GameRef) -> Option<Line<'static>> {
    let saves = &app.session().saves().saves;
    let mark = app.games().save_mark(row, app.session())?;
    let newest = app
        .games()
        .saves_of(row)
        .iter()
        .filter_map(|r| saves.get(r)?.backup.as_ref().map(|b| b.mtime))
        .max();
    let backed = newest.map(|t| format!("backed up {}", text::time_ago(t)));
    let what = match mark {
        SaveMark::Conflict => Palette::Warn.span("changed on AP and in the library"),
        SaveMark::CardNewer => Palette::Warn.span(match backed {
            Some(b) => format!("{b} · AP is newer"),
            None => "on AP, not backed up".into(),
        }),
        SaveMark::BackedUp(_) => Palette::Good.span(backed.unwrap_or_else(|| "backed up".into())),
        SaveMark::LibraryOnly => {
            Palette::Muted.span(backed.unwrap_or_else(|| "in the library".into()))
        }
    };
    Some(kv("Saves", what))
}

/// The right side of the Games tab while the saves panel has focus.
pub(super) fn draw_save_panel(frame: &mut Frame, app: &mut App, area: Rect) {
    let title = app
        .games()
        .selected()
        .and_then(|r| r.name(app.session()))
        .unwrap_or_default();
    let [list_area, about_area] =
        Layout::vertical([Constraint::Min(4), Constraint::Length(9)]).areas(area);
    // One column of preformatted lines, so a file name can use the width.
    let width = list_area.width.saturating_sub(4) as usize;
    let label_w = width.saturating_sub(9 + 2 + 10 + 2 + 14).max(8);
    let columns =
        |label: &str, when: &str, size: &str| format!("{label:<label_w$}{when:>9}  {size:>10}  ");
    let lines = app.games().panel_lines(app.session());
    let saves = &app.session().saves().saves;
    let rows: Vec<Row> = lines
        .iter()
        .map(|line| {
            let Some(s) = saves.get(line.rel()) else {
                return Row::new(vec![Cell::from("?")]);
            };
            let copy = |label: &str, size: u64, mtime: i64, note: Span<'static>| {
                let text = columns(
                    &format!("    {label}"),
                    &text::time_ago(mtime),
                    &text::bytes(size),
                );
                Row::new(vec![Cell::from(Line::from(vec![Span::raw(text), note]))])
            };
            match line {
                SaveLine::File(rel) => Row::new(vec![
                    Cell::from(truncate(
                        &rel.file_name().unwrap_or_default().to_string_lossy(),
                        width,
                    ))
                    .style(Style::new().add_modifier(Modifier::BOLD)),
                ]),
                SaveLine::Card(_) => {
                    let c = s.card.as_ref().expect("listed with a card copy");
                    let note = match &s.action {
                        SaveAction::Pull { .. } => Palette::Warn.span("← newer"),
                        SaveAction::Conflict { .. } => Palette::Warn.span("! differs"),
                        _ => Palette::Faint.span("same as backup"),
                    };
                    copy("on AP", c.size, c.mtime, note)
                }
                SaveLine::Backup(_) => {
                    let b = s.backup.as_ref().expect("listed with a backup");
                    copy(
                        "library",
                        b.size,
                        b.mtime,
                        Palette::Faint.span("current backup"),
                    )
                }
                SaveLine::Version(_, id) => match s.versions.iter().find(|v| v.id == *id) {
                    Some(v) => copy("older", v.size, v.mtime, Palette::Faint.span("")),
                    None => Row::new(vec![Cell::from("    ?")]),
                },
            }
        })
        .collect();
    let table = Table::new(rows, [Constraint::Fill(1)])
        .header(Row::new([columns("SAVE", "WHEN", "SIZE")]).style(Palette::Faint.style()))
        .row_highlight_style(selected())
        .highlight_symbol("▸ ")
        .block(panel(truncate(
            &format!("SAVES  ·  {title}"),
            area.width.saturating_sub(4) as usize,
        )));
    let mut state = TableState::default().with_selected(app.games().panel());
    frame.render_stateful_widget(table, list_area, &mut state);
    details(frame, about_area, save_line_details(app));
}

/// What the selected line of the saves panel is, and what p and x do to it.
fn save_line_details(app: &App) -> Vec<Line<'static>> {
    let Some(line) = app.games().panel_line(app.session()) else {
        return Vec::new();
    };
    let Some(s) = app.session().saves().saves.get(line.rel()) else {
        return Vec::new();
    };
    let mut lines = vec![
        kv_text("Path", line.rel().display().to_string()),
        kv(
            "Status",
            match &s.action {
                SaveAction::Pull { .. } | SaveAction::Conflict { .. } => {
                    Palette::Warn.span(save_status(&s.action))
                }
                _ => Palette::Muted.span(save_status(&s.action)),
            },
        ),
    ];
    // What p and x would do to this line; the keys are in the key bar.
    let effects: &[&str] = match line {
        SaveLine::File(_) => {
            &["Trash moves the backup and every older version to the library's trash."]
        }
        SaveLine::Card(_) => &["Trash deletes it from AP; the library keeps a copy first."],
        SaveLine::Backup(_) => &[
            "Putting it back on AP keeps AP's copy as an older version.",
            "Trash moves it to the library's trash; older versions stay.",
        ],
        SaveLine::Version(..) => &[
            "Putting it back on AP keeps AP's copy as an older version.",
            "Trash moves it to the library's trash.",
        ],
    };
    lines.extend(effects.iter().map(|e| Line::from(Palette::Muted.span(*e))));
    lines
}

// ---------------------------------------------------------------------------
// The Memories tab

pub(super) fn draw_memories(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.session().snapshot().is_none() {
        frame.render_widget(loading(app), area);
        return;
    }
    let (filter_row, table_area, details_area) = split_tab(app, area);
    let compact = table_area.width < 72;
    let rows: Vec<Row> = app
        .memories()
        .pane()
        .rows()
        .iter()
        .filter_map(|i| app.session().saves().memories.get(*i))
        .map(|m| {
            let h = &m.memory.header;
            let where_ = memory_where(m);
            let color = match (m.on_card, m.backed_up) {
                (true, false) => Palette::Warn,
                (true, true) => Palette::Good,
                _ => Palette::Muted,
            };
            let mut cells = vec![
                match &h.platform {
                    Some(p) => platform_cell(p),
                    None => Cell::from("CART").style(Palette::Muted.style()),
                },
                Cell::from(memory_game(m)),
                Cell::from(where_).style(color.style()),
                Cell::from(h.core.clone().unwrap_or_default()).style(Style::new().dim()),
                Cell::from(text::time_ago(m.memory.mtime)).style(Style::new().dim()),
                Cell::from(text::bytes(m.memory.size)).style(Style::new().dim()),
            ];
            if compact {
                cells.truncate(3);
            }
            Row::new(cells)
        })
        .collect();
    let new = app
        .session()
        .saves()
        .memories
        .iter()
        .filter(|m| m.on_card && !m.backed_up)
        .count();
    let title = if new > 0 {
        format!("MEMORIES  ·  {new} not backed up")
    } else {
        "MEMORIES".to_string()
    };
    let table = if compact {
        styled_table(
            rows,
            [
                Constraint::Length(8),
                Constraint::Min(12),
                Constraint::Length(14),
            ],
            ["PLATFORM", "GAME", "WHERE"],
            title,
            table_area.width,
        )
    } else {
        styled_table(
            rows,
            [
                Constraint::Length(9),
                Constraint::Min(20),
                Constraint::Length(14),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(9),
            ],
            ["PLATFORM", "GAME", "WHERE", "CORE", "SAVED", "SIZE"],
            title,
            table_area.width,
        )
    };
    let pane = app.memories().pane();
    let sel = (!pane.rows().is_empty()).then_some(pane.sel());
    draw_filter_bar(frame, app, filter_row);
    render_table(frame, app, table_area, table, sel);

    let lines = match app.memories().selected(app.session()) {
        Some(m) => memory_details(app, m),
        None if app.session().saves().memories.is_empty() => {
            vec![Line::from(Palette::Muted.span(
                if app.session().pocket().is_some() {
                    "No Memories on the card or in the library."
                } else {
                    "No Memories in the library. Plug in the card to see its Memories."
                },
            ))]
        }
        None => vec![Line::from(Palette::Muted.span("No Memories match."))],
    };
    details(frame, details_area, lines);
}

fn memory_details(app: &App, m: &MemoryEntry) -> Vec<Line<'static>> {
    let h = &m.memory.header;
    let game = memory_game(m);
    let mut lines = vec![kv_text("Game", game.clone())];
    if let Some(name) = library_game(app.session(), m).filter(|n| *n != game) {
        lines.push(kv_text("Library", name));
    }
    if h.cartridge {
        lines.push(kv_text("Made with", "a cartridge"));
    } else {
        if let Some(core) = &h.core {
            lines.push(kv_text(
                "Core",
                match &h.core_version {
                    Some(v) => format!("{core} {v}"),
                    None => core.clone(),
                },
            ));
        }
        if let Some(author) = &h.author {
            lines.push(kv_text("Author", author.clone()));
        }
    }
    lines.push(kv_text("Path", m.memory.rel_path.display().to_string()));
    for c in &m.memory.companions {
        lines.push(kv_text(
            "With",
            c.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        ));
    }
    if !h.cartridge {
        lines.push(Line::from(""));
        lines.push(Line::from(
            Palette::Faint.span("Loads only in the core that made it."),
        ));
    }
    lines
}
