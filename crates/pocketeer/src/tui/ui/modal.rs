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
            } => {
                let mut all = lines.clone();
                all.push(String::new());
                all.push(format!("> {value}▌"));
                let shown = suggestions.len().min(SUGGESTIONS_SHOWN);
                if shown > 0 {
                    all.push(String::new());
                    for (i, s) in suggestions.iter().take(shown).enumerate() {
                        all.push(format!(
                            "{} {}{}",
                            if *highlight == Some(i) { "▸" } else { " " },
                            s.name,
                            if s.is_dir { "/" } else { "" }
                        ));
                    }
                    if suggestions.len() > shown {
                        all.push(format!("  … {} more", suggestions.len() - shown));
                    }
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

    let longest = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(title.chars().count() + 2)
        .max(hint.chars().count()) as u16;
    let width = (longest + 4).clamp(44, area.width.saturating_sub(4).max(44));
    // body + gap (+ hint) + buttons + borders
    let extra = if hint.is_empty() { 4 } else { 5 };
    let height = (lines.len() as u16 + extra).clamp(6, area.height.saturating_sub(2).max(6));
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

    let total_lines = lines.len() as u16;
    // Prompts keep their input line and suggestions in view; the rest
    // scroll from the top.
    let scroll = if matches!(modal, Modal::Input { .. }) {
        total_lines.saturating_sub(body.height)
    } else {
        scroll.min(total_lines.saturating_sub(body.height))
    };
    let text: Vec<Line> = lines.into_iter().map(Line::from).collect();
    frame.render_widget(
        Paragraph::new(text)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        body,
    );
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
