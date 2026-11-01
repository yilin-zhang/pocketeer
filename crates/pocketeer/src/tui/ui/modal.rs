//! Dialogs: plan confirmations, info, prompts, picks, checklists, help.

use super::*;

use crate::tui::app::SUGGESTIONS_SHOWN;

pub(super) fn draw_modal(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(modal) = &app.modal else {
        return;
    };
    // (label, key hint, is_primary, focused)
    type Button = (&'static str, &'static str, bool, bool);
    let (title, lines, scroll, hint, buttons): (String, Vec<String>, u16, &str, Vec<Button>) =
        match modal {
            Modal::Confirm {
                title,
                lines,
                scroll,
                focus_apply,
                ..
            } => (
                title.clone(),
                lines.clone(),
                *scroll,
                "",
                vec![
                    ("Apply", "y", true, *focus_apply),
                    ("Cancel", "n / Esc", false, !*focus_apply),
                ],
            ),
            Modal::Info {
                title,
                lines,
                scroll,
            } => (
                title.clone(),
                lines.clone(),
                *scroll,
                "",
                vec![("Close", "Esc", false, true)],
            ),
            Modal::Input {
                title,
                lines,
                value,
                kind,
                suggestions,
                highlight,
                searching,
                ..
            } => {
                let mut all = lines.clone();
                all.push(String::new());
                all.push(format!("> {value}▌"));
                if kind.completes_paths() {
                    // Fixed rows: results and long paths never move the input.
                    let rect = path_rect(area);
                    let columns = rect.width.saturating_sub(4) as usize;
                    all = lines.iter().take(2).cloned().collect();
                    all.resize(2, String::new());
                    all.push(String::new());
                    all.push(format!(
                        "> {}▌",
                        path_tail(value, columns.saturating_sub(3))
                    ));
                    all.push(String::new());
                    let slots = rect.height.saturating_sub(11) as usize;
                    let offset = highlight
                        .unwrap_or(0)
                        .saturating_sub(slots.saturating_sub(1));
                    for i in offset..offset + slots {
                        all.push(
                            suggestions
                                .get(i)
                                .map(|s| {
                                    format!(
                                        "{} {}{}",
                                        if *highlight == Some(i) { "▸" } else { " " },
                                        s.name,
                                        if s.is_dir { "/" } else { "" }
                                    )
                                })
                                .unwrap_or_default(),
                        );
                    }
                    all.push(if *searching {
                        "Searching…".into()
                    } else if suggestions.is_empty() {
                        "No matching entries".into()
                    } else if suggestions.len() > slots {
                        format!("… {} more", suggestions.len() - slots)
                    } else {
                        String::new()
                    });
                }
                let hint = if kind.completes_paths() {
                    "↓ pick a match · Tab completes · Enter takes it, or submits"
                } else {
                    ""
                };
                (
                    title.clone(),
                    all,
                    0,
                    hint,
                    vec![("OK", "Enter", true, true), ("Cancel", "Esc", false, false)],
                )
            }
            Modal::Pick {
                title,
                items,
                selected,
                ..
            } => (
                title.clone(),
                items
                    .iter()
                    .enumerate()
                    .map(|(i, it)| format!("{} {it}", if i == *selected { "▸" } else { " " }))
                    .collect(),
                selected.saturating_sub(5) as u16,
                "↑↓ choose",
                vec![
                    ("Choose", "Enter", true, true),
                    ("Cancel", "Esc", false, false),
                ],
            ),
            Modal::Toggle {
                title,
                items,
                cursor,
                ..
            } => (
                title.clone(),
                items
                    .iter()
                    .enumerate()
                    .map(|(i, (label, on))| {
                        format!(
                            "{} {} {label}",
                            if i == *cursor { "▸" } else { " " },
                            if *on { "◼" } else { "◻" }
                        )
                    })
                    .collect(),
                cursor.saturating_sub(5) as u16,
                "Space toggles · a all / none · ↑↓ move",
                vec![
                    ("Save", "Enter", true, true),
                    ("Cancel", "Esc", false, false),
                ],
            ),
        };
    let is_confirm = matches!(modal, Modal::Confirm { .. });
    let interactive = !matches!(modal, Modal::Info { .. });

    let path_prompt = matches!(modal, Modal::Input { kind, .. } if kind.completes_paths());
    let longest = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(title.chars().count() + 2)
        .max(hint.chars().count()) as u16;
    let width = if path_prompt {
        path_rect(area).width
    } else {
        longest
            .saturating_add(4)
            .max(44)
            .min(area.width.saturating_sub(4).max(1))
    };
    let text: Vec<Line> = lines.iter().cloned().map(Line::from).collect();
    let paragraph = Paragraph::new(text);
    let paragraph = if path_prompt {
        paragraph
    } else {
        paragraph.wrap(Wrap { trim: false })
    };
    let body_width = width.saturating_sub(4);
    let total_lines = paragraph.line_count(body_width).min(u16::MAX as usize) as u16;
    // body + gap (+ hint) + buttons + borders
    let extra = if hint.is_empty() { 4 } else { 5 };
    let height = if path_prompt {
        path_rect(area).height
    } else {
        total_lines
            .saturating_add(extra)
            .max(6)
            .min(area.height.saturating_sub(2).max(1))
    };
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if interactive { MUTED } else { RULE }))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(INK).add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::horizontal(1));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let [body, hint_row, button_row] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if hint.is_empty() { 1 } else { 2 }),
        Constraint::Length(1),
    ])
    .areas(inner);

    // Prompts keep their input line and suggestions in view; the rest
    // scroll from the top.
    let selected_item = match modal {
        Modal::Pick { selected, .. } => Some(*selected),
        Modal::Toggle { cursor, .. } => Some(*cursor),
        _ => None,
    };
    let scroll = if path_prompt {
        0
    } else if let Some(selected) = selected_item {
        let rows = |line: &String| {
            Paragraph::new(line.as_str())
                .wrap(Wrap { trim: false })
                .line_count(body.width)
        };
        let start: usize = lines.iter().take(selected).map(rows).sum();
        let end = start + lines.get(selected).map_or(0, rows);
        end.saturating_sub(body.height as usize)
            .min(start)
            .min(u16::MAX as usize) as u16
    } else if matches!(modal, Modal::Input { .. }) {
        total_lines.saturating_sub(body.height)
    } else {
        scroll.min(total_lines.saturating_sub(body.height))
    };
    frame.render_widget(paragraph.scroll((scroll, 0)), body);
    if total_lines > body.height {
        let mut state = ScrollbarState::new(total_lines.saturating_sub(body.height) as usize)
            .position(scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .track_symbol(Some("│"))
                .thumb_symbol("┃")
                .track_style(Style::new().fg(RULE))
                .thumb_style(Style::new().fg(MUTED))
                .begin_symbol(None)
                .end_symbol(None),
            body,
            &mut state,
        );
    }
    if !hint.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(faint(hint)).centered()),
            Rect {
                y: hint_row.y + 1,
                height: 1,
                ..hint_row
            },
        );
    }

    // Buttons, centred: "  APPLY  y     CANCEL  esc". The focused one is a
    // white bar; its key hint sits beside it. Clickable; Enter presses it.
    let labels: Vec<(String, String)> = buttons
        .iter()
        .map(|(label, key, _, _)| (format!("  {}  ", label.to_uppercase()), format!(" {key}")))
        .collect();
    let total: u16 = labels
        .iter()
        .map(|(l, k)| (l.chars().count() + k.chars().count()) as u16)
        .sum::<u16>()
        + 4 * labels.len().saturating_sub(1) as u16;
    let mut x = button_row.x + button_row.width.saturating_sub(total) / 2;
    app.hit.buttons.clear();
    for ((_, _, primary, focused), (label, key)) in buttons.iter().zip(&labels) {
        let w = (label.chars().count() + key.chars().count()) as u16;
        let r = Rect {
            x,
            y: button_row.y,
            width: w.min(button_row.width),
            height: 1,
        };
        let style = match (*focused, *primary, is_confirm) {
            (true, true, true) => selected().bg(WARN),
            (true, _, _) => selected(),
            (false, true, _) => Style::new().fg(INK).bg(KEYCAP),
            (false, false, _) => Style::new().fg(MUTED).bg(KEYCAP),
        };
        let line = Line::from(vec![Span::styled(label.clone(), style), faint(key.clone())]);
        frame.render_widget(Paragraph::new(line), r);
        app.hit.buttons.push((r, *primary));
        x += w + 4;
    }
}

// Geometry depends only on terminal dimensions, never query/result text.
fn path_rect(area: Rect) -> Rect {
    let width = 100.min(area.width.saturating_sub(4).max(1));
    let height = (SUGGESTIONS_SHOWN as u16 + 11).min(area.height.saturating_sub(2).max(1));
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn path_tail(value: &str, columns: usize) -> String {
    let mut used = 0;
    let mut tail = Vec::new();
    for ch in value.chars().rev() {
        used += Line::from(ch.to_string()).width();
        if used > columns {
            break;
        }
        tail.push(ch);
    }
    tail.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cli::Global,
        tui::app::{InputKind, Suggestion},
    };
    use ratatui::{Terminal, backend::TestBackend};
    use ratatui_image::picker::Picker;

    #[test]
    fn path_prompt_geometry_and_input_row_stay_fixed_as_results_change() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::new(
            Global {
                json: false,
                root: Some(tmp.path().join("no-card")),
                offline: true,
                refresh: false,
                cache_dir: Some(tmp.path().join("cache")),
                library: Some(tmp.path().join("lib")),
                config: Some(tmp.path().join("config")),
                github_token: None,
            },
            Picker::halfblocks(),
        );
        for (width, height) in [(80, 24), (120, 40), (60, 18)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut positions = Vec::new();
            for count in [0, 1, 60] {
                app.modal = Some(Modal::Input {
                    title: "Import cover".into(),
                    lines: vec!["Choose an image".into(), "Tab completes".into()],
                    value: format!("/{}cover.webp", "目录/".repeat(count)),
                    kind: InputKind::ImportCover { game_id: 1 },
                    suggestions: (0..count)
                        .map(|i| Suggestion {
                            path: format!("/file{i}"),
                            name: "long filename ".repeat(count + 1),
                            is_dir: false,
                        })
                        .collect(),
                    highlight: None,
                    completion: None,
                    searching: count == 0,
                    tab_pending: false,
                });
                terminal
                    .draw(|f| draw_modal(f, &mut app, f.area()))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let position = |symbol: &str| {
                    (0..height)
                        .flat_map(|y| (0..width).map(move |x| (x, y)))
                        .find(|&(x, y)| buffer[(x, y)].symbol() == symbol)
                        .unwrap()
                };
                positions.push((
                    position("╭"),
                    position(">"),
                    position("▌"),
                    app.hit.buttons[0].0,
                ));
            }
            for other in &positions[1..] {
                assert_eq!(positions[0].0, other.0);
                assert_eq!(positions[0].1, other.1);
                assert_eq!(positions[0].2.1, other.2.1);
                assert_eq!(positions[0].3, other.3);
            }
        }
    }
}
