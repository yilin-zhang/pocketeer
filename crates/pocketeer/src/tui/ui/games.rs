//! The Games tab: library and card-only ROMs, cover and details.

use super::*;

pub(super) fn draw_games(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.snapshot.is_none() {
        frame.render_widget(loading(app), area);
        return;
    }
    let (filter_row, table_area, right) = split_tab(app, area);
    // Right: cover on top, details below.
    let [cover_area, details_area] = split_rows(app, right, Splitter::GamesRows);
    let snap = app.snapshot.as_ref().expect("checked above");
    let rows: Vec<Row> = app
        .games
        .rows
        .iter()
        .map(|r| {
            if let Some(g) = app.game_row(*r) {
                let verified = g.game.roms.iter().all(|r| r.verified);
                let size: u64 = g.game.roms.iter().map(|r| r.size).sum();
                let where_ = if g.on_card {
                    Cell::from("lib + card").style(Style::new().fg(GOOD))
                } else {
                    Cell::from("lib")
                };
                let dat = if verified {
                    Cell::from("✓").style(Style::new().fg(GOOD))
                } else {
                    Cell::from("?").style(Style::new().dim())
                };
                Row::new(vec![
                    Cell::from(g.game.platform.clone()).style(Style::new().dim()),
                    Cell::from(g.game.name.clone()),
                    where_,
                    dat,
                    Cell::from(if g.cover.is_some() { "▣" } else { "" }).style(Style::new().dim()),
                    Cell::from(human::bytes(size)).style(Style::new().dim()),
                ])
            } else if let Some(o) = app.card_rom(*r) {
                let (where_, color) = if o.name_in_library {
                    ("card ≠ lib", BAD)
                } else {
                    ("card", WARN)
                };
                Row::new(vec![
                    Cell::from(o.platform.clone()).style(Style::new().dim()),
                    Cell::from(pocketeer_core::roms::rename::stem(&o.name).to_string())
                        .style(Style::new().fg(WARN)),
                    Cell::from(where_).style(Style::new().fg(color)),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(human::bytes(o.size)).style(Style::new().dim()),
                ])
            } else {
                Row::new(vec![Cell::from("?")])
            }
        })
        .collect();
    let card_only = snap.card.as_ref().map_or(0, |c| c.card_roms.len());
    let title = match &snap.library {
        Some(l) => format!(
            "LIBRARY  ·  {} games  ·  {} saves{}  ·  {}",
            l.games.len(),
            l.saves,
            if card_only == 0 {
                String::new()
            } else {
                format!("  ·  {card_only} only on the card")
            },
            tilde(&l.root)
        ),
        None => format!(
            "LIBRARY  ·  none yet (n creates {})",
            app.library_path_display()
        ),
    };
    let table = styled_table(
        rows,
        [
            Constraint::Length(9),
            Constraint::Min(24),
            Constraint::Length(11),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(9),
        ],
        ["PLATFORM", "GAME", "WHERE", "DAT", "ART", "SIZE"],
        title,
        table_area.width,
    );
    let sel = (!app.games.rows.is_empty()).then_some(app.games.sel);
    draw_filter_bar(frame, app, filter_row);
    render_table(frame, app, table_area, table, sel);

    let selected = app
        .selected_game()
        .map(|g| (g.game.clone(), g.cover.clone(), g.on_card));
    let orphan = app.selected_orphan().cloned();
    let has_library = app.library_view().is_some();
    match (&selected, &orphan) {
        (None, Some(o)) => placeholder(
            frame,
            cover_area,
            &o.name,
            "on the card, not in the library",
            &[
                ("i", "copy it into the library"),
                ("I", "copy every card-only ROM"),
                ("x", "delete it from the card"),
            ],
        ),
        (Some((game, Some(cover_path), _)), _) => {
            let block = panel(truncate(&game.name, 40));
            let inner = block.inner(cover_area);
            frame.render_widget(block, cover_area);
            match app.cover_for(cover_path) {
                Some(proto) => frame.render_stateful_widget(
                    StatefulImage::<StatefulProtocol>::new(),
                    inner,
                    proto,
                ),
                None => frame.render_widget(
                    Paragraph::new(dim("cover file unreadable")).centered(),
                    inner,
                ),
            }
        }
        (Some((game, None, _)), _) => placeholder(
            frame,
            cover_area,
            &game.name,
            "no cover yet",
            &[
                ("f", "fetch it"),
                ("F", "fetch the whole platform"),
                ("C", "fetch everything missing"),
            ],
        ),
        (None, None) if !has_library => placeholder(
            frame,
            cover_area,
            "LIBRARY",
            "no library yet",
            &[
                (
                    "n",
                    &format!("create one at {}", app.library_path_display()),
                ),
                ("a", "then import ROMs from a folder"),
                ("i", "or from the card"),
            ],
        ),
        (None, None) => placeholder(
            frame,
            cover_area,
            "LIBRARY",
            "no games match",
            &[
                ("a", "import ROMs from a folder"),
                ("I", "import ROMs that are only on the card"),
                ("l", "change what the list shows"),
            ],
        ),
    }

    let mut lines: Vec<Line> = Vec::new();
    if let Some(o) = &orphan {
        lines.push(kv_text("File", o.name.clone()));
        lines.push(kv_text("Platform", o.platform.clone()));
        lines.push(kv_text("Card path", o.rel_path.display().to_string()));
        lines.push(kv_text("Size", human::bytes(o.size)));
        lines.push(kv("Library", fg("not imported  (i imports it)", WARN)));
        if o.name_in_library {
            lines.push(Line::from(fg(
                "A library ROM has this name with different bytes; importing will skip it.",
                BAD,
            )));
        }
    }
    if let Some((game, cover, on_card)) = &selected {
        lines.push(kv_text("Game", game.name.clone()));
        lines.push(kv_text("Platform", game.platform.clone()));
        for (i, rom) in game.roms.iter().enumerate() {
            if game.roms.len() > 1 {
                lines.push(Line::from(Span::styled(
                    format!("ROM {} of {}", i + 1, game.roms.len()),
                    Style::new().bold(),
                )));
            }
            lines.push(kv_text("File", rom.name.clone()));
            lines.push(kv_text("Size", human::bytes(rom.size)));
            lines.push(kv(
                "DAT",
                if rom.verified {
                    fg("verified", GOOD)
                } else {
                    dim("not checked against a DAT")
                },
            ));
            lines.push(kv("crc32", dim(rom.crc32.clone())));
            lines.push(kv_text(
                "Kept as",
                match rom.mode.as_str() {
                    "copy" => "copy in the library".to_string(),
                    "link" => "hard link in the library".to_string(),
                    _ => format!(
                        "reference to {}",
                        rom.source_path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    ),
                },
            ));
        }
        lines.push(kv(
            "On card",
            if *on_card {
                fg(format!("yes, Assets/{}/common/", game.platform), GOOD)
            } else {
                Span::raw("no  (S syncs the device)")
            },
        ));
        lines.push(kv_text(
            "Cover",
            if cover.is_some() {
                "yes"
            } else {
                "none yet  (f fetches it)"
            },
        ));
    }
    details(frame, details_area, lines);
}

/// Where a cover would be: a dot-matrix field (the Pocket's screen) with a
/// short line and the keys that apply, centred.
fn placeholder(frame: &mut Frame, area: Rect, title: &str, what: &str, keys: &[(&str, &str)]) {
    let block = panel(truncate(title, 40));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let dots: Vec<Line> = (0..inner.height)
        .map(|y| {
            let row: String = (0..inner.width)
                .map(|x| if (x + y) % 2 == 0 { '·' } else { ' ' })
                .collect();
            Line::from(Span::styled(row, Style::new().fg(RULE)))
        })
        .collect();
    frame.render_widget(Paragraph::new(dots), inner);

    let mut lines = vec![Line::from(dim(what.to_string())), Line::from("")];
    lines.extend(keys.iter().map(|(k, what)| key_line(&[(k, what)])));
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
