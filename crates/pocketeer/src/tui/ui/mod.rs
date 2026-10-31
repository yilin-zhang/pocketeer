//! Drawing. A pure function of `App`, except that it records where things
//! were drawn (`App::hit`) so clicks can be mapped back.

use pocketeer_core::required::FileStatus;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Cell, Clear, Gauge, Padding, Paragraph, Row, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Table, TableState, Tabs, Wrap,
    },
};
use ratatui_image::{StatefulImage, protocol::StatefulProtocol};

use super::app::{App, BiosFilter, Cycle, Modal, Splitter, Tab};
use crate::human;

mod bios;
mod cores;
mod device;
mod games;
mod modal;

use bios::draw_bios;
use cores::draw_cores;
use device::draw_device;
use games::draw_games;
use modal::draw_modal;

// Palette. Analogue OS is black and white; colour is kept for state (good,
// warn, bad) so it means something when it shows up. Indexed colours render
// the same in every 256-colour terminal.
const INK: Color = Color::Indexed(255);
const PAPER: Color = Color::Indexed(232);
const MUTED: Color = Color::Indexed(246);
const FAINT: Color = Color::Indexed(240);
const RULE: Color = Color::Indexed(237);
const KEYCAP: Color = Color::Indexed(238);
const ACCENT: Color = INK;
const WARN: Color = Color::Indexed(179);
const BAD: Color = Color::Indexed(167);
const GOOD: Color = Color::Indexed(114);

/// Width of the label column in detail panes.
const LABEL: usize = 10;

/// The selected table row, the active filter and the focused dialog button:
/// one solid white bar, like a hardware menu. It overrides every cell's own
/// colour so the row reads as one line.
fn selected() -> Style {
    Style::new()
        .bg(INK)
        .fg(PAPER)
        .add_modifier(Modifier::BOLD)
        .remove_modifier(Modifier::DIM)
}

/// A panel: rounded hairline border, small muted title.
fn panel(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(RULE))
        .title(Span::styled(
            format!(" {} ", title.into()),
            Style::new().fg(MUTED),
        ))
}

/// A key legend entry: the key on a dark cap, what it does beside it.
fn keycap(key: &str) -> Span<'static> {
    Span::styled(
        format!(" {key} "),
        Style::new().fg(INK).bg(KEYCAP).add_modifier(Modifier::BOLD),
    )
}

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

    // Hit areas are rebuilt every frame by the drawing code below.
    app.hit = super::app::HitAreas::default();
    draw_header(frame, app, header);
    match app.tab {
        Tab::Cores => draw_cores(frame, app, body),
        Tab::Bios => draw_bios(frame, app, body),
        Tab::Games => draw_games(frame, app, body),
        Tab::Device => draw_device(frame, app, body),
    }
    draw_footer(frame, app, footer);
    if app.modal.is_some() {
        draw_modal(frame, app, frame.area());
    }
}

// ---------------------------------------------------------------------------
// small helpers

fn dim(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(MUTED))
}

fn faint(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(FAINT))
}

fn fg(s: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(color))
}

/// `Label      value` with a dim, fixed-width label.
fn kv(label: &str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![faint(format!("{label:<LABEL$}")), value])
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
    app.hit.rows = table_rows(area);
    app.hit.offset = state.offset();
}

fn styled_table<'a, const N: usize>(
    rows: Vec<Row<'a>>,
    widths: [Constraint; N],
    header: [&'static str; N],
    title: String,
    width: u16,
) -> Table<'a> {
    Table::new(rows, widths)
        .header(Row::new(header).style(Style::new().fg(FAINT)))
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
    let pct = app.splits.get(Splitter::Cols, app.tab);
    let [left, right] = Layout::horizontal([
        Constraint::Percentage(pct),
        Constraint::Percentage(100 - pct),
    ])
    .areas(area);
    // The two touching borders are the grab area.
    let grab = Rect::new(left.right().saturating_sub(1), area.y, 2, area.height);
    app.hit.splitters.push((grab, area, Splitter::Cols));
    [left, right]
}

/// Top / bottom at a draggable split.
fn split_rows(app: &mut App, area: Rect, which: Splitter) -> [Rect; 2] {
    let pct = app.splits.get(which, app.tab);
    let [top, bottom] = Layout::vertical([
        Constraint::Percentage(pct),
        Constraint::Percentage(100 - pct),
    ])
    .areas(area);
    let grab = Rect::new(area.x, top.bottom().saturating_sub(1), area.width, 2);
    app.hit.splitters.push((grab, area, which));
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
    app.hit.tabs = tabs_row;

    // Left: the mark. Right: where we are, as a few muted chips.
    let brand = Line::from(vec![
        Span::styled(" ◆ ", Style::new().fg(INK)),
        Span::styled(
            "POCKETEER",
            Style::new().fg(INK).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(brand), title_row);

    let mut chips: Vec<Span> = Vec::new();
    let chip = |spans: &mut Vec<Span>, s: Span<'static>| {
        if !spans.is_empty() {
            spans.push(faint("  ·  "));
        }
        spans.push(s);
    };
    match (&app.pocket, app.card_view()) {
        (Some(p), card) => {
            let dot = if let Some(d) = app.library_view().and_then(|l| l.device.as_ref()) {
                Span::styled(format!("● {}", d.name), Style::new().fg(GOOD))
            } else {
                Span::styled("● card", Style::new().fg(INK))
            };
            chip(&mut chips, dot);
            let name = p
                .root()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| tilde(p.root()));
            chip(&mut chips, dim(truncate(&name, 24)));
            if let Some(c) = card {
                if let Some(fw) = c.status.firmware.as_ref().and_then(|f| f.version.clone()) {
                    chip(&mut chips, dim(format!("fw {fw}")));
                }
                if let Some(d) = c.status.disk {
                    chip(
                        &mut chips,
                        dim(format!("{} free", human::bytes(d.available_bytes))),
                    );
                }
            }
        }
        // No card is a normal state, not an error: the library is still here.
        (None, _) => chip(&mut chips, faint("○ no SD card")),
    }
    if let Some(l) = app.library_view() {
        chip(&mut chips, dim(format!("{} games", l.games.len())));
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
                Style::new().fg(FAINT),
            ))
        })
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.tab.index())
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
        pane.query().to_string(),
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
        format!(" / {query}▌")
    } else if query.is_empty() {
        " / filter".to_string()
    } else {
        format!(" / {query}")
    };
    let query_style = if editing {
        Style::new().fg(INK)
    } else if query.is_empty() {
        Style::new().fg(FAINT)
    } else {
        Style::new().fg(INK)
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
            Style::new().fg(FAINT)
        };
        let r = push(&mut spans, Span::styled(format!(" {label} "), style));
        segments.push((r, i));
        push(&mut spans, Span::raw(" "));
    }
    push(&mut spans, faint(format!("  {count}")));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    app.hit.query = query_rect;
    app.hit.segments = segments;
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let [status_row, keys_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    match (&app.busy, app.download_pct) {
        (Some(b), Some(pct)) => {
            let [label, bar] =
                Layout::horizontal([Constraint::Length(28), Constraint::Min(10)]).areas(status_row);
            frame.render_widget(
                Paragraph::new(fg(format!(" {} {b}", spinner()), INK)),
                label,
            );
            frame.render_widget(
                Gauge::default()
                    .gauge_style(Style::new().fg(INK).bg(RULE))
                    .ratio((pct as f64 / 100.0).clamp(0.0, 1.0))
                    .label(format!("{pct}%")),
                bar,
            );
        }
        _ => {
            let mut status = match (&app.busy, app.last_log()) {
                (Some(b), _) => Line::from(fg(
                    match app.busy_step {
                        Some((done, total)) => format!(" {} {b}  {done}/{total}", spinner()),
                        None => format!(" {} {b}", spinner()),
                    },
                    INK,
                )),
                (None, Some(l)) => Line::from(vec![faint(" › "), dim(l.to_string())]),
                (None, None) => Line::from(""),
            };
            if let Some(side) = &app.side_status {
                status.push_span(dim(format!("   {} {side}", spinner())));
            }
            frame.render_widget(Paragraph::new(status), status_row);
        }
    }

    let keys: &[(&str, &str)] = match app.tab {
        Tab::Cores => &[
            ("Enter", "install / update"),
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
        Tab::Games => &[
            ("a", "import"),
            ("i", "from card"),
            ("e", "rename"),
            ("t", "standard names"),
            ("x", "remove"),
            ("f", "cover"),
            ("S", "sync"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Device => &[
            ("D", "subscriptions"),
            ("S", "sync"),
            ("V", "saves"),
            ("c", "clean junk"),
            ("?", "help"),
            ("q", "quit"),
        ],
    };
    frame.render_widget(Paragraph::new(key_line(keys)), keys_row);
}

/// ` [key] description  [key] description …`: a button legend.
fn key_line(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (k, what) in keys {
        spans.push(keycap(k));
        spans.push(dim(format!(" {what}  ")));
    }
    Line::from(spans)
}

/// Draws the placeholder a card tab shows before the first load or without
/// a card; `false` means the tab has nothing else to draw.
fn card_tab(frame: &mut Frame, app: &App, area: Rect) -> bool {
    if app.snapshot.is_none() {
        frame.render_widget(loading(app), area);
    } else if app.card_view().is_none() {
        frame.render_widget(no_card(app), area);
    } else {
        return true;
    }
    false
}

/// The body before the first load has landed.
fn loading(app: &App) -> Paragraph<'static> {
    let text = match &app.busy {
        Some(b) => format!("{} {b}…", spinner()),
        None => format!("{} Loading…", spinner()),
    };
    Paragraph::new(Line::from(dim(text)))
        .centered()
        .block(panel("").padding(Padding::top(2)))
}

/// The body of a card tab while no SD card is mounted.
fn no_card(app: &App) -> Paragraph<'static> {
    let mut lines = vec![
        Line::from(Span::styled(
            "No SD card mounted.",
            Style::new().fg(INK).bold(),
        )),
        Line::from(""),
        Line::from("Plug the Pocket's card in (or mount the Pocket): it is picked up"),
        Line::from("by itself, no key needed. This tab fills in then."),
        Line::from(""),
        Line::from("Meanwhile the Games tab (3) manages the library on this computer:"),
        Line::from("import ROMs, rename, fetch covers. Changes reach the card on the"),
        Line::from("next sync (S)."),
    ];
    if let Some(e) = app
        .pocket_error
        .as_ref()
        .filter(|e| e.code != "SD_NOT_FOUND")
    {
        lines.push(Line::from(""));
        lines.push(fg(e.message.clone(), WARN).into());
        if let Some(h) = &e.hint {
            lines.push(dim(h.clone()).into());
        }
    }
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(panel("CARD").padding(Padding::new(2, 2, 1, 1)))
}
