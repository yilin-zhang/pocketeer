//! Dialogs: plan confirmations, info, prompts, picks, checklists, help.

use super::*;

use crate::tui::app::SUGGESTIONS_SHOWN;

/// One path suggestion: the typed text stands out in the name; the picked
/// row is the selection bar.
fn suggestion_line(s: &crate::tui::app::Suggestion, picked: bool) -> Line<'static> {
    let base = if picked {
        selected()
    } else {
        Palette::Muted.style()
    };
    let hit = if picked {
        base.add_modifier(Modifier::UNDERLINED)
    } else {
        Palette::Ink.style().add_modifier(Modifier::BOLD)
    };
    let mut spans = vec![Span::styled(if picked { "▸ " } else { "  " }, base)];
    match s.matched {
        Some((a, b))
            if b <= s.name.len() && s.name.is_char_boundary(a) && s.name.is_char_boundary(b) =>
        {
            spans.push(Span::styled(s.name[..a].to_string(), base));
            spans.push(Span::styled(s.name[a..b].to_string(), hit));
            spans.push(Span::styled(s.name[b..].to_string(), base));
        }
        _ => spans.push(Span::styled(s.name.clone(), base)),
    }
    if s.is_dir {
        spans.push(Span::styled("/", base));
    }
    Line::from(spans)
}

pub(super) fn draw_modal(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(modal) = app.modal() else {
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
                paths,
                ..
            } => {
                let mut all = lines.clone();
                all.push(String::new());
                all.push(format!("> {}", edit_view(value, usize::MAX)));
                if let Some(paths) = paths {
                    let suggestions = &paths.suggestions;
                    // Fixed rows: results and long paths never move the input.
                    let rect = path_rect(area);
                    let columns = rect.width.saturating_sub(4) as usize;
                    all = lines.iter().take(2).cloned().collect();
                    all.resize(2, String::new());
                    all.push(String::new());
                    all.push(format!("> {}", edit_view(value, columns.saturating_sub(2))));
                    all.push(String::new());
                    let slots = rect.height.saturating_sub(11) as usize;
                    // Drawn styled on top afterwards (see `suggestion_line`).
                    all.extend(std::iter::repeat_n(String::new(), slots));
                    all.push(if paths.searching {
                        "Searching…".into()
                    } else if suggestions.is_empty() {
                        "Nothing matches".into()
                    } else if suggestions.len() > slots {
                        format!("… {} more", suggestions.len() - slots)
                    } else {
                        String::new()
                    });
                }
                let hint = if paths.is_some() {
                    "↓ pick  ·  Tab completes"
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

    let path_prompt = matches!(modal, Modal::Input { step, .. } if step.completes_paths());
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
        .border_style(
            if interactive {
                Palette::Muted
            } else {
                Palette::Rule
            }
            .style(),
        )
        .title(Span::styled(
            format!(" {title} "),
            Palette::Ink.style().add_modifier(Modifier::BOLD),
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
    if let Modal::Input {
        paths: Some(paths), ..
    } = modal
    {
        let (suggestions, highlight) = (&paths.suggestions, &paths.highlight);
        // Rows 0–4 are the two text lines, a gap, the input and a gap.
        let slots = path_rect(area).height.saturating_sub(11) as usize;
        let offset = highlight
            .unwrap_or(0)
            .saturating_sub(slots.saturating_sub(1));
        let rows: Vec<Line> = (offset..offset + slots)
            .map(|i| match suggestions.get(i) {
                Some(s) => suggestion_line(s, *highlight == Some(i)),
                None => Line::from(""),
            })
            .collect();
        let list = Rect {
            y: body.y + 5,
            height: (slots as u16).min(body.height.saturating_sub(5)),
            ..body
        };
        frame.render_widget(Paragraph::new(rows), list);
    }
    if total_lines > body.height {
        let mut state = ScrollbarState::new(total_lines.saturating_sub(body.height) as usize)
            .position(scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .track_symbol(Some("│"))
                .thumb_symbol("┃")
                .track_style(Palette::Rule.style())
                .thumb_style(Palette::Muted.style())
                .begin_symbol(None)
                .end_symbol(None),
            body,
            &mut state,
        );
    }
    if !hint.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Palette::Faint.span(hint)).centered()),
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
    let mut hits = Vec::new();
    for ((_, _, primary, focused), (label, key)) in buttons.iter().zip(&labels) {
        let w = (label.chars().count() + key.chars().count()) as u16;
        let r = Rect {
            x,
            y: button_row.y,
            width: w.min(button_row.width),
            height: 1,
        };
        let style = match (*focused, *primary, is_confirm) {
            (true, true, true) => selected().bg(Palette::Warn.color()),
            (true, _, _) => selected(),
            (false, true, _) => Palette::Ink.style().bg(Palette::Keycap.color()),
            (false, false, _) => Palette::Muted.style().bg(Palette::Keycap.color()),
        };
        let line = Line::from(vec![
            Span::styled(label.clone(), style),
            Palette::Faint.span(key.clone()),
        ]);
        frame.render_widget(Paragraph::new(line), r);
        hits.push((r, *primary));
        x += w + 4;
    }
    app.layout_mut().record_buttons(hits);
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

/// A text field with its cursor (`▌`) in `columns`: the text before the
/// cursor keeps its end in view, the text after fills what is left.
pub(super) fn edit_view(value: &crate::tui::app::line_edit::LineEdit, columns: usize) -> String {
    let (before, after) = value.split();
    let room = columns.saturating_sub(1);
    let shown = path_tail(before, room);
    let used = Line::from(shown.as_str()).width();
    let mut rest = String::new();
    let mut width = used;
    for ch in after.chars() {
        let w = Line::from(ch.to_string()).width();
        if width + w > room {
            break;
        }
        width += w;
        rest.push(ch);
    }
    format!("{shown}▌{rest}")
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
    use crate::tui::app::{CoverStep, Step, Suggestion};
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn path_prompt_geometry_and_input_row_stay_fixed_as_results_change() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = crate::tui::app::test_app(tmp.path());
        for (width, height) in [(80, 24), (120, 40), (60, 18)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut positions = Vec::new();
            for count in [0, 1, 60] {
                let dialog = app.dialog_mut();
                dialog.input(
                    "Import cover",
                    vec!["Choose an image".into(), "Tab completes".into()],
                    format!("/{}cover.webp", "目录/".repeat(count)),
                    Step::Cover(CoverStep::Image { game_id: 1 }),
                );
                dialog.set_suggestions(
                    (0..count)
                        .map(|i| Suggestion {
                            path: format!("/file{i}"),
                            name: "long filename ".repeat(count + 1),
                            is_dir: false,
                            matched: None,
                        })
                        .collect(),
                );
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
                    app.layout().button(0).unwrap(),
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
