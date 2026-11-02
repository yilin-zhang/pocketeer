//! Drawing. A pure function of `App`, except that it records where things
//! were drawn (in `Layout`) so clicks can be mapped back.

use pocketeer_core::card::bios::FileStatus;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Cell, Clear, Gauge, Padding, Paragraph, Row, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Table, TableState, Tabs, Wrap,
    },
};
use ratatui_image::{StatefulImage, protocol::StatefulProtocol};

use super::app::{App, BiosFilter, Cycle, Modal, Splitter, Tab};
use crate::text;

mod card_tabs;
mod games;
mod modal;
mod saves;
mod theme;

use card_tabs::{draw_bios, draw_cores, draw_device};
use games::draw_games;
use modal::draw_modal;
use saves::draw_memories;
use theme::*;

/// Width of the label column in detail panes.
const LABEL: usize = 10;

/// One of four frames, by wall clock, for "something is running".
fn spinner() -> &'static str {
    const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    FRAMES[((ms / 150) % 4) as usize]
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    // Hit areas are recorded every frame by the drawing code below.
    app.layout_mut().begin_frame();
    draw_header(frame, app, header);
    match app.tab() {
        Tab::Cores => draw_cores(frame, app, body),
        Tab::Bios => draw_bios(frame, app, body),
        Tab::Games => draw_games(frame, app, body),
        Tab::Memories => draw_memories(frame, app, body),
        Tab::Device => draw_device(frame, app, body),
    }
    draw_footer(frame, app, footer);
    if app.modal().is_some() {
        draw_modal(frame, app, frame.area());
    }
}

// ---------------------------------------------------------------------------
// small helpers

/// `Label      value` with a dim, fixed-width label.
fn kv(label: &str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![Palette::Faint.span(format!("{label:<LABEL$}")), value])
}

fn kv_text(label: &str, value: impl Into<String>) -> Line<'static> {
    kv(label, Span::raw(value.into()))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn details(frame: &mut Frame, area: Rect, lines: Vec<Line>) {
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel("DETAILS")),
        area,
    );
}

/// The data rows of a bordered table with a one-line header.
fn table_rows(area: Rect) -> Rect {
    let inner = Block::bordered().inner(area);
    Rect {
        y: inner.y.saturating_add(1),
        height: inner.height.saturating_sub(1),
        ..inner
    }
}

/// Renders a table with the pane's selection and records its hit area.
fn render_table(frame: &mut Frame, app: &mut App, area: Rect, table: Table, sel: Option<usize>) {
    let mut state = TableState::default().with_selected(sel);
    frame.render_stateful_widget(table, area, &mut state);
    app.layout_mut()
        .record_table(table_rows(area), state.offset());
}

fn styled_table<'a, const N: usize>(
    rows: Vec<Row<'a>>,
    widths: [Constraint; N],
    header: [&'static str; N],
    title: String,
    width: u16,
) -> Table<'a> {
    Table::new(rows, widths)
        .header(Row::new(header).style(Palette::Faint.style()))
        .row_highlight_style(selected())
        .highlight_symbol("▸ ")
        .block(panel(truncate(
            title.trim(),
            width.saturating_sub(4) as usize,
        )))
}

/// A tab: the filter bar across the top, then a table on the left and a
/// details column on the right.
fn split_tab(app: &mut App, area: Rect) -> (Rect, Rect, Rect) {
    let [filter_row, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let [table_area, right] = split_cols(app, body);
    (filter_row, table_area, right)
}

/// Left | right at the current tab's draggable split.
fn split_cols(app: &mut App, area: Rect) -> [Rect; 2] {
    let pct = app.layout().split(Splitter::Cols, app.tab());
    let [left, right] = Layout::horizontal([
        Constraint::Percentage(pct),
        Constraint::Percentage(100 - pct),
    ])
    .areas(area);
    // The two touching borders are the grab area.
    let grab = Rect::new(left.right().saturating_sub(1), area.y, 2, area.height);
    app.layout_mut().record_splitter(grab, area, Splitter::Cols);
    [left, right]
}

/// Top / bottom at a draggable split.
fn split_rows(app: &mut App, area: Rect, which: Splitter) -> [Rect; 2] {
    let pct = app.layout().split(which, app.tab());
    let [top, bottom] = Layout::vertical([
        Constraint::Percentage(pct),
        Constraint::Percentage(100 - pct),
    ])
    .areas(area);
    let grab = Rect::new(area.x, top.bottom().saturating_sub(1), area.width, 2);
    app.layout_mut().record_splitter(grab, area, which);
    [top, bottom]
}

/// `/Users/me/x` → `~/x`.
fn tilde(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    match dirs::home_dir() {
        Some(h) => match s.strip_prefix(&h.display().to_string()) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
            _ => s,
        },
        None => s,
    }
}

// ---------------------------------------------------------------------------
// header, filter bar, footer

fn draw_header(frame: &mut Frame, app: &mut App, area: Rect) {
    let [title_row, tabs_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    app.layout_mut().record_tabs(tabs_row);

    // Left: the mark. Right: where we are, as a few muted chips.
    let brand = Line::from(vec![
        Span::styled(" ◆ ", Palette::Ink.style()),
        Span::styled(
            "POCKETEER",
            Palette::Ink.style().add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(brand), title_row);

    let mut chips: Vec<Span> = Vec::new();
    let chip = |spans: &mut Vec<Span>, s: Span<'static>| {
        if !spans.is_empty() {
            spans.push(Palette::Faint.span("  ·  "));
        }
        spans.push(s);
    };
    // Where the card is and how much room it has; the rest is on the Card tab.
    match (app.session().pocket(), app.session().card()) {
        (Some(p), card) => {
            let name = p
                .root()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| tilde(p.root()));
            chip(
                &mut chips,
                Span::styled(format!("● {}", truncate(&name, 24)), Palette::Ink.style()),
            );
            if let Some(d) = card.and_then(|c| c.status.disk) {
                chip(
                    &mut chips,
                    Palette::Muted.span(format!("{} free", text::bytes(d.available_bytes))),
                );
            }
        }
        // No card is a normal state, not an error: the library is still here.
        (None, _) => chip(&mut chips, Palette::Faint.span("○ no SD card")),
    }
    chips.push(Span::raw(" "));
    frame.render_widget(
        Paragraph::new(Line::from(chips)).alignment(ratatui::layout::Alignment::Right),
        title_row,
    );

    let titles: Vec<Line> = Tab::ALL
        .iter()
        .map(|t| {
            Line::from(Span::styled(
                format!(" {} ", t.label()),
                Palette::Faint.style(),
            ))
        })
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.tab().index())
        .highlight_style(selected())
        .divider("  ")
        .padding("", "");
    frame.render_widget(tabs, tabs_row);
}

/// One line: `/ query▌   show: [all] in library  on card …   n`. The query
/// and every segment are clickable.
fn draw_filter_bar(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(pane) = app.pane() else {
        return;
    };
    let (query, editing, labels, current, count) = (
        pane.query_text(),
        pane.editing(),
        pane.filter_labels(),
        pane.filter_index(),
        pane.len(),
    );
    let mut spans: Vec<Span> = Vec::new();
    let mut x = area.x;
    let mut push = |spans: &mut Vec<Span>, span: Span<'static>| -> Rect {
        let w = span.content.chars().count() as u16;
        let r = Rect {
            x,
            y: area.y,
            width: w,
            height: 1,
        };
        x = x.saturating_add(w);
        spans.push(span);
        r
    };

    let query_text = if editing {
        format!(" / {query}")
    } else if query.is_empty() {
        " / filter".to_string()
    } else {
        format!(" / {query}")
    };
    let query_style = if editing {
        Palette::Ink.style()
    } else if query.is_empty() {
        Palette::Faint.style()
    } else {
        Palette::Ink.style()
    };
    let mut query_rect = push(&mut spans, Span::styled(query_text, query_style));
    // Pad so the segments don't jump around while typing.
    let pad = 18u16.saturating_sub(query_rect.width);
    push(&mut spans, Span::raw(" ".repeat(pad as usize)));
    query_rect.width += pad;

    let mut segments = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        let style = if i == current {
            selected()
        } else {
            Palette::Faint.style()
        };
        let r = push(&mut spans, Span::styled(format!(" {label} "), style));
        segments.push((r, i));
        push(&mut spans, Span::raw(" "));
    }
    push(&mut spans, Palette::Faint.span(format!("  {count}")));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    app.layout_mut().record_filter_bar(query_rect, segments);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let [status_row, keys_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    let busy = app.session().busy();
    match (&busy.main, busy.download_pct) {
        (Some(b), Some(pct)) => {
            let [label, bar] =
                Layout::horizontal([Constraint::Length(28), Constraint::Min(10)]).areas(status_row);
            frame.render_widget(
                Paragraph::new(Palette::Ink.span(format!(" {} {b}", spinner()))),
                label,
            );
            frame.render_widget(
                Gauge::default()
                    .gauge_style(Palette::Ink.style().bg(Palette::Rule.color()))
                    .ratio((pct as f64 / 100.0).clamp(0.0, 1.0))
                    .label(format!("{pct}%")),
                bar,
            );
        }
        _ => {
            let mut status = match (&busy.main, app.session().log().next()) {
                (Some(b), _) => Line::from(Palette::Ink.span(match busy.step {
                    Some((done, total)) => format!(" {} {b}  {done}/{total}", spinner()),
                    None => format!(" {} {b}", spinner()),
                })),
                (None, Some(l)) => Line::from(vec![
                    Palette::Faint.span(" › "),
                    Palette::Muted.span(l.to_string()),
                ]),
                (None, None) => Line::from(""),
            };
            if let Some(roms) = &busy.roms {
                status.push_span(Palette::Muted.span(format!("   {} {roms}", spinner())));
            }
            if let Some(side) = &busy.side {
                status.push_span(Palette::Muted.span(format!("   {} {side}", spinner())));
            }
            frame.render_widget(Paragraph::new(status), status_row);
        }
    }

    // Legend order, same on every tab, so keys of one kind sit together
    // and their tints (see `keycap`) form runs: open/back, library, card,
    // names and covers, remove, filter, then help and quit. Keep new keys
    // in this order.
    let keys: &[(&str, &str)] = match app.tab() {
        Tab::Games if app.games().panel().is_some() => &[
            ("Esc", "back"),
            ("b", "back up"),
            ("B", "back up all saves"),
            ("p", "put back on AP"),
            ("x", "trash"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Games => &games_keys(app),
        Tab::Memories => &[
            ("b", "back up"),
            ("B", "back up all"),
            ("p", "put back on AP"),
            ("x", "trash"),
            ("/", "filter"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Cores => &[
            ("u", "install / update"),
            ("U", "update all"),
            ("x", "remove"),
            ("/", "search"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Bios => &[
            ("p", "place files from card root"),
            ("/", "filter"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Device => &[("x", "clean junk"), ("?", "help"), ("q", "quit")],
    };
    frame.render_widget(
        Paragraph::new(key_line(keys, keys_row.width as usize)),
        keys_row,
    );
}

/// The Games legend: what applies to the selected row first, then the
/// tab-wide keys. Cover, DAT and naming keys are in the cover panel and `?`.
fn games_keys(app: &App) -> Vec<(&'static str, &'static str)> {
    // In the legend order above.
    let s = app.session();
    if s.library().is_none() {
        return vec![("n", "create the library"), ("?", "help"), ("q", "quit")];
    }
    let row = app.games().selected();
    let game = row.and_then(|r| r.game(s));
    let has_saves = row.is_some_and(|r| !app.games().saves_of(r).is_empty());
    let mut keys = Vec::new();
    if has_saves {
        keys.push(("Enter", "saves"));
        keys.push(("b", "back up saves"));
    }
    if s.pocket().is_some() {
        keys.push(("B", "back up all saves"));
    }
    if row.is_some() && game.is_none() {
        keys.push(("i", "copy into library"));
    }
    keys.push(("a", "import ROMs"));
    match (row, game) {
        (Some(_), Some(g)) => {
            if !g.on_card() {
                keys.push(("s", "copy to card"));
            }
            keys.push(("e", "rename"));
            keys.push(("x", "remove"));
        }
        (Some(_), None) => keys.push(("x", "delete from card")),
        _ => {}
    }
    keys.push(("?", "help"));
    keys.push(("q", "quit"));
    keys
}

/// ` [key] description  [key] description …`: a button legend. When it is
/// wider than `width`, keys are left out from the end of the list rather
/// than cut mid-word; help and quit always stay, since `?` lists the rest.
fn key_line(keys: &[(&str, &str)], width: usize) -> Line<'static> {
    fn entry(key: &str, what: &str) -> [Span<'static>; 2] {
        [keycap(key), Palette::Muted.span(format!(" {what}  "))]
    }
    fn entry_width(e: &[Span<'static>; 2]) -> usize {
        e.iter().map(Span::width).sum()
    }
    let kept = |key: &str| matches!(key, "?" | "q");
    let reserved: usize = keys
        .iter()
        .filter(|(k, _)| kept(k))
        .map(|(k, w)| entry_width(&entry(k, w)))
        .sum();
    let mut room = width.saturating_sub(1 + reserved);
    let mut spans = vec![Span::raw(" ")];
    for (k, what) in keys.iter().filter(|(k, _)| !kept(k)) {
        let e = entry(k, what);
        let w = entry_width(&e);
        if w > room {
            break;
        }
        room -= w;
        spans.extend(e);
    }
    for (k, what) in keys.iter().filter(|(k, _)| kept(k)) {
        spans.extend(entry(k, what));
    }
    Line::from(spans)
}

/// Draws the placeholder a card tab shows before the first load or without
/// a card; `false` means the tab has nothing else to draw.
fn card_tab(frame: &mut Frame, app: &App, area: Rect) -> bool {
    if app.session().snapshot().is_none() {
        frame.render_widget(loading(app), area);
    } else if app.session().card().is_none() {
        frame.render_widget(no_card(app), area);
    } else {
        return true;
    }
    false
}

/// The body before the first load has landed.
fn loading(app: &App) -> Paragraph<'static> {
    let text = match &app.session().busy().main {
        Some(b) => format!("{} {b}…", spinner()),
        None => format!("{} Loading…", spinner()),
    };
    Paragraph::new(Line::from(Palette::Muted.span(text)))
        .centered()
        .block(panel("").padding(Padding::top(2)))
}

/// The body of a card tab while no SD card is mounted.
fn no_card(app: &App) -> Paragraph<'static> {
    let mut lines = vec![
        Line::from(Span::styled(
            "No SD card mounted.",
            Palette::Ink.style().bold(),
        )),
        Line::from(""),
        Line::from("Plug the Pocket's card in (or mount the Pocket): it is picked up"),
        Line::from("by itself, no key needed. This tab fills in then."),
        Line::from(""),
        Line::from("Meanwhile the Games tab (1) manages the library on this computer:"),
        Line::from("import ROMs, rename, fetch covers. Changes reach the card on the"),
        Line::from("next sync (s)."),
    ];
    if let Some(e) = app
        .session()
        .pocket_error()
        .filter(|e| e.code != "SD_NOT_FOUND")
    {
        lines.push(Line::from(""));
        lines.push(Palette::Warn.span(e.message.clone()).into());
        if let Some(h) = &e.hint {
            lines.push(Palette::Muted.span(h.clone()).into());
        }
    }
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(panel("CARD").padding(Padding::new(2, 2, 1, 1)))
}

#[cfg(test)]
mod key_line_tests {
    use super::key_line;

    fn text(width: usize) -> String {
        let keys = [
            ("a", "import"),
            ("s", "copy to card"),
            ("x", "remove"),
            ("?", "help"),
            ("q", "quit"),
        ];
        key_line(&keys, width)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn a_narrow_legend_drops_keys_from_the_end_but_keeps_help_and_quit() {
        let wide = text(200);
        assert!(wide.contains("copy to card") && wide.contains("quit"));
        let narrow = text(50);
        assert!(narrow.len() <= 50 + 10, "{narrow}");
        assert!(narrow.contains("import") && !narrow.contains("copy"));
        assert!(
            narrow.contains("help") && narrow.contains("quit"),
            "{narrow}"
        );
    }
}
