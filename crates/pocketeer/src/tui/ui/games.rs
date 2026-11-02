//! The Games tab: library and card-only ROMs, cover and details.

use super::saves::{draw_save_panel, save_cell, save_summary};
use super::*;
use crate::tui::app::covers::{self, Cover};
use pocketeer_core::{card::roms::Confidence, transfer::Presence};

pub(super) fn draw_games(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.session().snapshot().is_none() {
        frame.render_widget(loading(app), area);
        return;
    }
    let (filter_row, table_area, right) = split_tab(app, area);
    // Right: cover on top, details below; or the game's saves.
    let panel_open = app.games().panel().is_some();
    let [cover_area, details_area] = if panel_open {
        [Rect::default(), Rect::default()]
    } else {
        split_rows(app, right, Splitter::GamesRows)
    };
    let s = app.session();
    let snap = s.snapshot().expect("checked above");
    let compact = table_area.width < 68;
    let game_row = |mut cells: Vec<Cell<'static>>| {
        if compact {
            cells.truncate(3);
        }
        Row::new(cells)
    };
    let rows: Vec<Row> = app
        .games()
        .pane()
        .rows()
        .iter()
        .map(|r| {
            if let Some(g) = r.game(s) {
                let size: u64 = g.game.roms.iter().map(|r| r.size).sum();
                let where_ = if g.on_card() {
                    Cell::from("lib + card").style(Palette::Good.style())
                } else if g.matches.iter().any(|m| m.presence == Presence::Unknown) {
                    Cell::from("lib + ?").style(Palette::Warn.style())
                } else {
                    Cell::from("lib")
                };
                use pocketeer_core::library::identify::DatStatus;
                let states: Vec<_> = g.game.roms.iter().map(|r| r.dat.status).collect();
                let (symbol, color) = if states.iter().all(|s| *s == DatStatus::Matched) {
                    ("✓", Palette::Good)
                } else if states.contains(&DatStatus::Error) {
                    ("!", Palette::Warn)
                } else if states.contains(&DatStatus::Unchecked) {
                    ("?", Palette::Muted)
                } else if states.contains(&DatStatus::Ambiguous) {
                    ("~", Palette::Warn)
                } else if states.contains(&DatStatus::NoMatch) {
                    ("–", Palette::Muted)
                } else {
                    ("n/a", Palette::Muted)
                };
                let dat = Cell::from(symbol).style(color.style());
                game_row(vec![
                    platform_cell(&g.game.platform),
                    Cell::from(format!(
                        "{}{}",
                        g.game
                            .roms
                            .first()
                            .map(|r| r.name.as_str())
                            .unwrap_or(&g.game.name),
                        if g.matches
                            .iter()
                            .flat_map(|m| &m.copies)
                            .any(|c| c.name_mismatch)
                        {
                            " ≠"
                        } else {
                            ""
                        }
                    )),
                    where_,
                    dat,
                    Cell::from(if g.cover.is_some() { "▣" } else { "" }).style(Style::new().dim()),
                    save_cell(app, *r),
                    Cell::from(text::bytes(size)).style(Style::new().dim()),
                ])
            } else if let Some(o) = r.card_rom(s) {
                let (where_, color) = match o.confidence {
                    Confidence::Pending => ("card + ?", Palette::Warn),
                    Confidence::Error => ("read error", Palette::Bad),
                    _ => ("card", Palette::Warn),
                };
                game_row(vec![
                    platform_cell(&o.platform),
                    Cell::from(pocketeer_core::names::stem(&o.name).to_string())
                        .style(Palette::Warn.style()),
                    Cell::from(where_).style(color.style()),
                    Cell::from(""),
                    Cell::from(""),
                    save_cell(app, *r),
                    Cell::from(text::bytes(o.size)).style(Style::new().dim()),
                ])
            } else {
                game_row(vec![Cell::from("?")])
            }
        })
        .collect();
    let title = match &snap.library {
        Some(l) => format!("LIBRARY  ·  {} games  ·  {}", l.games.len(), tilde(&l.root)),
        None => format!(
            "LIBRARY  ·  none yet (n creates {})",
            s.library_path_display()
        ),
    };
    let table = if compact {
        styled_table(
            rows,
            [
                Constraint::Length(8),
                Constraint::Min(10),
                Constraint::Length(10),
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
                Constraint::Min(24),
                Constraint::Length(11),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(4),
                Constraint::Length(9),
            ],
            ["PLATFORM", "GAME", "WHERE", "DAT", "ART", "SAVE", "SIZE"],
            title,
            table_area.width,
        )
    };
    let pane = app.games().pane();
    let sel = (!pane.rows().is_empty()).then_some(pane.sel());
    draw_filter_bar(frame, app, filter_row);
    render_table(frame, app, table_area, table, sel);
    if panel_open {
        draw_save_panel(frame, app, right);
        return;
    }
    let summary = app.games().selected().and_then(|r| save_summary(app, r));

    let s = app.session();
    let selected = app
        .games()
        .selected_game(s)
        .map(|g| (g.game.clone(), g.cover.clone()));
    let orphan = app.games().selected_card_rom(s).cloned();
    let selected_matches = app
        .games()
        .selected_game(s)
        .map(|g| g.matches.clone())
        .unwrap_or_default();
    let has_library = s.library().is_some();
    let library_path = s.library_path_display();
    match (&selected, &orphan) {
        (None, Some(_)) => placeholder(frame, cover_area, "COVER", "on the card only", &[]),
        (Some((_, Some(cover_path))), _) => {
            let block = panel("COVER");
            let inner = block.inner(cover_area);
            frame.render_widget(block, cover_area);
            match app.cover(cover_path, inner.as_size()) {
                Cover::Dots(lines) => frame.render_widget(Paragraph::new(lines.to_vec()), inner),
                Cover::Image(proto) => frame.render_stateful_widget(
                    StatefulImage::<StatefulProtocol>::new().resize(covers::RESIZE),
                    inner,
                    proto,
                ),
                Cover::Loading => frame.render_widget(
                    Paragraph::new(Palette::Muted.span("loading cover…")).centered(),
                    inner,
                ),
                Cover::Unreadable => frame.render_widget(
                    Paragraph::new(Palette::Muted.span("cover file unreadable")).centered(),
                    inner,
                ),
            }
        }
        (Some((_, None)), _) => placeholder(
            frame,
            cover_area,
            "COVER",
            "no cover yet",
            &[("f", "download it"), ("c", "use an image file")],
        ),
        (None, None) if !has_library => placeholder(
            frame,
            cover_area,
            "LIBRARY",
            &format!("no library yet; n creates it at {}", library_path),
            &[],
        ),
        (None, None) => placeholder(frame, cover_area, "LIBRARY", "no games match", &[]),
    }

    let mut lines: Vec<Line> = Vec::new();
    if let Some(o) = &orphan {
        lines.push(kv_text("AP path", o.rel_path.display().to_string()));
        lines.extend(summary.clone());
        lines.push(kv_text("Identity", format!("{:?}", o.confidence)));
        if let Some(sha1) = &o.sha1 {
            lines.push(kv_text("SHA-1", sha1.clone()));
        }
        if let Some(error) = &o.error {
            lines.push(Line::from(Palette::Bad.span(error.clone())));
        }
    }
    if let Some((game, _)) = &selected {
        // The table has platform, where, size and cover; this adds the rest.
        lines.push(kv_text("Game", game.name.clone()));
        lines.extend(summary);
        for matched in &selected_matches {
            for copy in &matched.copies {
                lines.push(kv_text("AP path", copy.path.display().to_string()));
                lines.push(kv_text("Identity", format!("{:?}", copy.confidence)));
                if copy.name_mismatch {
                    lines.push(Line::from(
                        Palette::Muted.span("≠ names differ; e reviews rename options"),
                    ));
                }
            }
        }
        for (i, rom) in game.roms.iter().enumerate() {
            if game.roms.len() > 1 {
                lines.push(Line::from(Span::styled(
                    format!("ROM {} of {}", i + 1, game.roms.len()),
                    Style::new().bold(),
                )));
            }
            let multi = game.roms.len() > 1;
            if multi || pocketeer_core::names::stem(&rom.name) != game.name {
                lines.push(kv_text("File", rom.name.clone()));
            }
            if multi {
                lines.push(kv_text("Size", text::bytes(rom.size)));
            }
            // The DAT column shows a match; only other states need words.
            if rom.dat.status != pocketeer_core::library::identify::DatStatus::Matched {
                lines.push(kv_text("DAT", rom.dat.status.label().to_string()));
            }
            if let Some(name) = rom.dat.game_name.as_ref().filter(|n| **n != game.name) {
                lines.push(kv_text("Standard", name.clone()));
            }
            if let Some(error) = &rom.dat.error {
                lines.push(kv_text("DAT retry", error.clone()));
            }
            lines.push(kv("crc32", Palette::Muted.span(rom.crc32.clone())));
        }
    }
    details(frame, details_area, lines);
}

/// Where a cover would be: a dot-matrix field (the Pocket's screen) with a
/// short line and the cover's own keys, centred. A key is listed in one
/// place only, where it acts: cover keys here, row and tab keys in the key
/// bar.
fn placeholder(frame: &mut Frame, area: Rect, title: &str, what: &str, keys: &[(&str, &str)]) {
    let block = panel(truncate(title, 40));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let dots: Vec<Line> = (0..inner.height)
        .map(|y| {
            let row: String = (0..inner.width)
                .map(|x| if (x + y) % 2 == 0 { '·' } else { ' ' })
                .collect();
            Line::from(Span::styled(row, Palette::Rule.style()))
        })
        .collect();
    frame.render_widget(Paragraph::new(dots), inner);

    let mut lines = vec![Line::from(Palette::Muted.span(what.to_string()))];
    if !keys.is_empty() {
        lines.push(Line::from(""));
        lines.extend(
            keys.iter().map(|(k, what)| {
                Line::from(vec![keycap(k), Palette::Muted.span(format!(" {what}"))])
            }),
        );
    }
    let height = lines.len() as u16;
    let width = lines
        .iter()
        .map(|l| l.width() as u16)
        .max()
        .unwrap_or(0)
        .min(inner.width);
    let text = Rect {
        x: inner.x + inner.width.saturating_sub(width) / 2,
        y: inner.y + inner.height.saturating_sub(height) / 2,
        width,
        height: height.min(inner.height),
    };
    frame.render_widget(Clear, text);
    frame.render_widget(Paragraph::new(lines), text);
}
