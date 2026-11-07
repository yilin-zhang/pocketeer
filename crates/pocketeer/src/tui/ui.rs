//! Draws the `App`, and defines the palette.

use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use pocketeer_core::{
    cores::{BiosRow, BiosStatus, CoreRow},
    game::Files,
    platform::Platform,
    pocketeer::GameRow,
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Cell, Clear, HighlightSpacing, Paragraph, Row as TableRow, Table,
        TableState, Wrap,
    },
};
use ratatui_image::{StatefulImage, protocol::StatefulProtocol};
use strum::IntoEnumIterator;

use super::{
    app::{
        App, DeviceStatus, GamesFilter, ImportPrompt, ImportTarget, Page, PlatformPrompt, Prompt,
        RenamePrompt, RenameTarget, StatusMessage, name_of,
    },
    covers::{self, CoverDisplay},
    path_input::{PathEntry, PathInput},
    worker::GamesTableRow,
};

/// Width of the label column in the details.
const DETAILS_LABEL_WIDTH: usize = 10;

/// The palette. Analogue OS is black and white, so the UI is gray, and color is
/// kept for state so that it means something when it appears. Indexed colors
/// render the same in every 256-color terminal.
#[derive(Debug, Clone, Copy)]
enum Palette {
    // Grays, from text to background.
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
    // Keycap tints by what a key changes, muted and at one lightness, so a
    // legend reads as one bar with a few soft blocks.
    KeyLibrary,
    KeyDevice,
    KeyRemove,
}

impl Palette {
    const fn color(self) -> Color {
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
            Palette::KeyDevice => 60,
            Palette::KeyRemove => 95,
        })
    }

    const fn style(self) -> Style {
        Style::new().fg(self.color())
    }

    fn span(self, text: impl Into<String>) -> Span<'static> {
        Span::styled(text.into(), self.style())
    }
}

/// A platform as the table shows it, with its id in capitals in a color of its
/// own. The colors are soft so that they don't read as a state.
fn platform_cell(platform: &Platform) -> Cell<'static> {
    const OTHERS: [u8; 6] = [110, 146, 182, 144, 116, 181];
    let id = platform.id();
    let color = match platform {
        Platform::Gb => 143,
        Platform::Gbc => 175,
        Platform::Gba => 140,
        Platform::Nes => 174,
        Platform::Snes => 104,
        Platform::Genesis => 67,
        Platform::MasterSystem => 109,
        Platform::GameGear => 73,
        Platform::PcEngine => 180,
        Platform::NeoGeo | Platform::Other(_) => {
            OTHERS[id.bytes().map(usize::from).sum::<usize>() % OTHERS.len()]
        }
    };
    Cell::from(id.to_uppercase()).style(Style::new().fg(Color::Indexed(color)))
}

/// The style of the selected row and the focused button, a solid white bar like
/// a hardware menu.
fn selected_style() -> Style {
    Style::new()
        .bg(Palette::Ink.color())
        .fg(Palette::Paper.color())
        .add_modifier(Modifier::BOLD)
}

/// The selected row when it is also marked, a dimmer bar to tell it from an
/// unmarked one.
fn selected_and_marked_style() -> Style {
    selected_style().bg(Palette::Muted.color())
}

/// A panel with a rounded hairline border and a small muted title.
fn panel_block(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Palette::Rule.style())
        .title(Span::styled(
            format!(" {} ", title.into()),
            Palette::Muted.style(),
        ))
}

/// A key in a legend, in the tint of what it changes.
fn keycap(key: &str, tint: Palette) -> Span<'static> {
    Span::styled(
        format!(" {key} "),
        Palette::Ink
            .style()
            .bg(tint.color())
            .add_modifier(Modifier::BOLD),
    )
}

/// One of four frames, by the clock, showing that something is running.
fn spinner_frame() -> &'static str {
    const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    FRAMES[((milliseconds / 150) % 4) as usize]
}

fn labeled(label: &str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![
        Palette::Faint.span(format!("{label:<DETAILS_LABEL_WIDTH$}")),
        value,
    ])
}

fn labeled_text(label: &str, value: impl Into<String>) -> Line<'static> {
    labeled(label, Span::raw(value.into()))
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, search, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(4),
    ])
    .areas(frame.area());
    draw_header(frame, app, header);
    match app.current_page() {
        Page::Games if !app.games.is_loaded() => {
            draw_search(frame, app, search);
            frame.render_widget(
                Paragraph::new(
                    Palette::Muted.span(format!(" {} reading the library…", spinner_frame())),
                ),
                body,
            );
        }
        Page::Games => {
            draw_search(frame, app, search);
            draw_games_page(frame, app, body);
        }
        Page::Cores => {
            draw_cores_status(frame, app, search);
            draw_cores_page(frame, app, body);
        }
        Page::Bios => {
            draw_bios_status(frame, app, search);
            draw_bios_page(frame, app, body);
        }
        Page::Device => draw_device_page(frame, app, body),
    }
    draw_footer(frame, app, footer);
    match app.prompt() {
        Some(Prompt::Import(import)) => draw_import(frame, import),
        Some(Prompt::Platform(choice)) => draw_platform_prompt(frame, choice),
        Some(Prompt::Rename(rename)) => draw_rename(frame, rename),
        None => {}
    }
    if app.dialog().is_some() {
        draw_dialog(frame, app);
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let tab = |key: &str, name: &str, page: Page| {
        let text = format!("   {key} {name}");
        if app.current_page() == page {
            Palette::Ink.span(text)
        } else {
            Palette::Faint.span(text)
        }
    };
    let brand = Line::from(vec![
        Span::styled(" ◆ ", Palette::Ink.style()),
        Span::styled(
            "POCKETEER",
            Palette::Ink.style().add_modifier(Modifier::BOLD),
        ),
        tab("1", "Games", Page::Games),
        tab("2", "Cores", Page::Cores),
        tab("3", "BIOS", Page::Bios),
        tab("4", "Device", Page::Device),
    ]);
    frame.render_widget(Paragraph::new(brand), area);
    let name_of = |root: &Path| {
        let name = root.file_name().unwrap_or(root.as_os_str());
        name.to_string_lossy().into_owned()
    };
    let device = match app.device_status() {
        DeviceStatus::Reading(root) => {
            Palette::Muted.span(format!("{} reading {}…", spinner_frame(), name_of(root)))
        }
        DeviceStatus::Open(root) => Palette::Ink.span(format!("● {}", name_of(root))),
        DeviceStatus::NoDevice => Palette::Faint.span("○ no device"),
    };
    let (games, orphans) = app.games.game_and_orphan_counts();
    let mut chips = vec![device, Palette::Faint.span("  ·  ")];
    chips.push(Palette::Muted.span(format!("{games} games")));
    if orphans > 0 {
        chips.push(Palette::Faint.span("  ·  "));
        chips.push(Palette::Muted.span(format!("{orphans} orphan saves")));
    }
    chips.push(Span::raw(" "));
    frame.render_widget(
        Paragraph::new(Line::from(chips)).alignment(Alignment::Right),
        area,
    );
}

/// Text being typed, with the cursor showing.
fn typed_spans(before: String, after: &str) -> [Span<'static>; 3] {
    let mut after = after.chars();
    let at_cursor = after.next().unwrap_or(' ');
    [
        Palette::Ink.span(before),
        Span::styled(
            at_cursor.to_string(),
            Palette::Ink.style().add_modifier(Modifier::REVERSED),
        ),
        Palette::Ink.span(after.collect::<String>()),
    ]
}

fn draw_search(frame: &mut Frame, app: &App, area: Rect) {
    let mut filters = Line::from(keycap("f", Palette::Keycap));
    for filter in GamesFilter::iter() {
        let palette = match filter == app.games.filter() {
            true => Palette::Ink,
            false => Palette::Faint,
        };
        filters.push_span(palette.span(format!("  {filter} ")));
    }
    let [search_area, filter_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(filters.width() as u16),
    ])
    .areas(area);
    let mut search = if app.games.is_searching() {
        let (before, after) = app.games.search_text_around_cursor();
        let mut line = Line::from(Palette::Ink.span(" / "));
        line.extend(typed_spans(before, &after));
        line
    } else if app.games.search_text().is_empty() {
        Line::from(Palette::Faint.span(" / search"))
    } else {
        Line::from(Palette::Ink.span(format!(" / {}", app.games.search_text())))
    };
    search.push_span(Palette::Faint.span(format!("   {}", app.games.visible_len())));
    frame.render_widget(Paragraph::new(search), search_area);
    frame.render_widget(Paragraph::new(filters), filter_area);
}

/// The width of the column naming what is in the rows. It is as wide as the
/// longest name and gives way first in a narrow terminal, and the other columns
/// follow right after it.
fn names_column_width(header: &str, names: impl Iterator<Item = String>) -> Constraint {
    let longest = names.map(|name| Span::raw(name).width()).max();
    let width = longest.unwrap_or(0).max(header.len());
    Constraint::Max(width as u16)
}

/// The table of a page. The room for the selection stays when nothing is
/// selected, so every page's columns start at the same place.
fn page_table<'a>(
    title: &'static str,
    header: impl IntoIterator<Item = &'a str>,
    rows: Vec<TableRow<'a>>,
    widths: impl IntoIterator<Item = Constraint>,
) -> Table<'a> {
    Table::new(rows, widths)
        .header(TableRow::new(header).style(Palette::Faint.style()))
        .row_highlight_style(selected_style())
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always)
        .block(panel_block(title))
}

/// Shows how many updates there are, or why the inventory can't be fetched.
fn draw_cores_status(frame: &mut Frame, app: &App, area: Rect) {
    let Some(cores) = app.cores.core_list() else {
        return;
    };
    let updates = cores.rows.iter().filter(|row| row.has_update()).count();
    let line = match (&cores.inventory_failure, updates) {
        (Some(failure), _) => {
            Palette::Warn.span(format!(" couldn't get the cores to install: {failure}"))
        }
        (None, 0) => Palette::Faint.span(" every core is up to date"),
        (None, 1) => Palette::Muted.span(" 1 update"),
        (None, update_count) => Palette::Muted.span(format!(" {update_count} updates")),
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_cores_page(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(cores) = app.cores.core_list() else {
        let reading = Palette::Muted.span(" reading the cores…");
        frame.render_widget(Paragraph::new(reading).block(panel_block("CORES")), area);
        return;
    };
    let rows: Vec<TableRow> = cores.rows.iter().map(core_table_row).collect();
    let table = page_table(
        "CORES",
        ["PLATFORM", "CORE", "ON THE DEVICE", "LATEST"],
        rows,
        [
            Constraint::Length(9),
            names_column_width("CORE", cores.rows.iter().map(|row| row.id().to_string())),
            Constraint::Length(14),
            Constraint::Length(10),
        ],
    );
    frame.render_stateful_widget(table, area, &mut app.cores.table_state);
}

fn core_table_row(row: &CoreRow) -> TableRow<'static> {
    let platforms = match (row.latest(), row.installed()) {
        (Some(latest), _) => &latest.platforms[..],
        (None, Some(installed)) => &installed.platforms[..],
        (None, None) => &[],
    };
    let platform = platforms.first().map_or(Cell::from(""), platform_cell);
    let on_device = match row.installed().map(|i| i.version.clone()) {
        None => Cell::from("–").style(Palette::Muted.style()),
        Some(None) => Cell::from("unknown").style(Palette::Warn.style()),
        Some(Some(version)) if row.has_update() => Cell::from(version).style(Palette::Warn.style()),
        Some(Some(version)) => Cell::from(version).style(Palette::Good.style()),
    };
    let latest = row
        .latest()
        .map_or(String::new(), |latest| latest.version.clone());
    TableRow::new([
        platform,
        Cell::from(row.id().to_string()),
        on_device,
        Cell::from(latest),
    ])
}

/// Shows how many files aren't right, in red if a core can't run because of
/// them.
fn draw_bios_status(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.bios.rows();
    let missing = |row: &&BiosRow| row.status == BiosStatus::Missing;
    let is_wrong = |row: &&BiosRow| matches!(row.status, BiosStatus::WrongSize { .. });
    let wrong = rows.iter().filter(is_wrong).count();
    let line = if app.device_root().is_none() {
        Palette::Faint.span(" no device")
    } else if rows.is_empty() {
        Palette::Faint.span(" no core on the device needs a file")
    } else if rows.iter().all(|row| row.status == BiosStatus::Present) {
        Palette::Faint.span(" every file is there")
    } else {
        let palette = if wrong > 0 || rows.iter().filter(missing).any(|row| row.is_required()) {
            Palette::Bad
        } else {
            Palette::Warn
        };
        let missing = rows.iter().filter(missing).count();
        palette.span(format!(" {missing} missing, {wrong} of the wrong size"))
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_device_page(frame: &mut Frame, app: &App, area: Rect) {
    let known = |value: Option<String>| value.unwrap_or_else(|| "unknown".to_string());
    let lines = match (app.device_status(), &app.device_page) {
        (DeviceStatus::Reading(_), _) => vec![Line::from(
            Palette::Muted.span(format!("{} reading the device…", spinner_frame())),
        )],
        (_, None) => vec![Line::from(Palette::Faint.span("no device"))],
        (_, Some(data)) => {
            let firmware = data.firmware.as_ref().map(ToString::to_string);
            let space = data.disk_space.as_ref().map(ToString::to_string);
            let cores = data.core_count.as_ref().map(ToString::to_string);
            vec![
                labeled_text("Root", data.root.display().to_string()),
                labeled_text("Firmware", known(firmware)),
                labeled_text("Space", known(space)),
                labeled_text("Cores", known(cores)),
                labeled_text("Games", data.game_count.to_string()),
            ]
        }
    };
    frame.render_widget(Paragraph::new(lines).block(panel_block("DEVICE")), area);
}

fn draw_bios_page(frame: &mut Frame, app: &mut App, area: Rect) {
    let rows: Vec<TableRow> = app.bios.rows().iter().map(bios_table_row).collect();
    let files = app
        .bios
        .rows()
        .iter()
        .map(|row| row.path.display().to_string());
    let table = page_table(
        "BIOS",
        ["PLATFORM", "FILE", "STATUS", "NEEDED BY"],
        rows,
        [
            Constraint::Length(9),
            names_column_width("FILE", files),
            Constraint::Length(11),
            Constraint::Min(16),
        ],
    );
    frame.render_stateful_widget(table, area, &mut app.bios.table_state);
}

fn bios_table_row(row: &BiosRow) -> TableRow<'static> {
    let (status, palette) = match row.status {
        BiosStatus::Present => ("present", Palette::Good),
        BiosStatus::WrongSize { .. } => ("wrong size", Palette::Bad),
        BiosStatus::Missing if row.is_required() => ("missing", Palette::Bad),
        BiosStatus::Missing => ("missing", Palette::Warn),
    };
    let cores: Vec<&str> = row
        .needed_by
        .iter()
        .map(|(core, _)| core.as_str())
        .collect();
    TableRow::new([
        platform_cell(&row.platform),
        Cell::from(row.path.display().to_string()),
        Cell::from(status).style(palette.style()),
        Cell::from(cores.join(", ")),
    ])
}

fn draw_games_page(frame: &mut Frame, app: &mut App, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area);
    draw_games_table(frame, app, left);
    match app.games.selected_version_index() {
        Some(selected) => draw_saves(frame, app, right, selected),
        None => {
            let [cover, details] =
                Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .areas(right);
            draw_cover(frame, app, cover);
            draw_details(frame, app, details);
        }
    }
}

fn draw_games_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let compact = area.width < 60;
    let rows: Vec<TableRow> = app
        .games
        .visible_rows()
        .map(|row| game_table_row(app, row, compact))
        .collect();
    let table = if compact {
        page_table(
            "GAMES",
            vec!["PLATFORM", "GAME"],
            rows,
            vec![Constraint::Length(9), Constraint::Min(10)],
        )
    } else {
        page_table(
            "GAMES",
            vec!["PLATFORM", "GAME", "WHERE", "ART", "SAVE"],
            rows,
            vec![
                Constraint::Length(9),
                names_column_width("GAME", app.games.rows().iter().map(name_of)),
                Constraint::Length(12),
                Constraint::Length(3),
                Constraint::Length(4),
            ],
        )
    };
    let table = match app.games.selected_row() {
        Some(row) if app.games.is_marked(row) => {
            table.row_highlight_style(selected_and_marked_style())
        }
        _ => table,
    };
    frame.render_stateful_widget(table, area, &mut app.games.table_state);
}

/// A column's only symbol, in the middle of it.
fn centered_cell(text: &'static str) -> Cell<'static> {
    Cell::from(Text::from(text).centered())
}

fn game_table_row(app: &App, row: &GamesTableRow, compact: bool) -> TableRow<'static> {
    let name = Cell::from(name_of(row));
    let mut cells = match row {
        GamesTableRow::Game(game_row) => {
            let place = match game_row {
                GameRow::Both { .. } => Cell::from("lib + device").style(Palette::Good.style()),
                GameRow::InLibrary(_) => Cell::from("lib"),
                GameRow::OnDevice(_) => Cell::from("device"),
            };
            let art = match game_row.local_entry() {
                Some(local) if local.cover_path.is_some() => {
                    centered_cell("▣").style(Palette::Good.style())
                }
                Some(_) => centered_cell("–").style(Palette::Muted.style()),
                None => Cell::from(""),
            };
            let has_device_save = game_row
                .device_entry()
                .is_some_and(|on_device| on_device.save.is_some());
            let save = if game_row.has_new_save_on_device() {
                centered_cell("←").style(Palette::Warn.style())
            } else if has_device_save {
                centered_cell("✓").style(Palette::Good.style())
            } else if game_row.saves_kept() > 0 {
                centered_cell("·").style(Palette::Muted.style())
            } else {
                Cell::from("")
            };
            let platform = platform_cell(&game_row.game().platform);
            vec![platform, name, place, art, save]
        }
        GamesTableRow::OrphanSaveInLibrary(orphan) => vec![
            platform_cell(&orphan.platform),
            name,
            Cell::from("lib save"),
            Cell::from(""),
            centered_cell("?"),
        ],
        GamesTableRow::OrphanSaveOnDevice(orphan) => vec![
            platform_cell(&orphan.platform),
            name,
            Cell::from("device save"),
            Cell::from(""),
            centered_cell("?"),
        ],
    };
    if compact {
        cells.truncate(2);
    }
    let marked = Style::new().bg(Palette::Rule.color());
    let style = match (row, app.games.is_marked(row)) {
        (GamesTableRow::Game(_), false) => Style::new(),
        (GamesTableRow::Game(_), true) => marked,
        // Gray all over, because they are saves with no game.
        (_, false) => Palette::Faint.style(),
        // Lighter than that, to show on the mark's gray.
        (_, true) => marked.fg(Palette::Muted.color()),
    };
    TableRow::new(cells).style(style)
}

fn draw_cover(frame: &mut Frame, app: &mut App, area: Rect) {
    let local = app
        .games
        .selected_row()
        .and_then(GamesTableRow::local_entry);
    let path = local.and_then(|local| local.cover_path.clone());
    let around = app.games.covers_around();
    let block = panel_block("COVER");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(path) = path else {
        let (what, keys): (_, &[KeyHint]) = match app.games.selected_row() {
            Some(GamesTableRow::Game(GameRow::OnDevice(_))) => ("on the device only", &[]),
            Some(GamesTableRow::Game(_)) => (
                "no cover yet",
                &[("a", "download it"), ("c", "use an image file")],
            ),
            Some(_) => ("", &[]),
            None => ("no games", &[]),
        };
        draw_cover_placeholder(frame, inner, what, keys);
        return;
    };
    match app.cover_loader.show(&path, &around, inner.as_size()) {
        CoverDisplay::Dots(lines) => frame.render_widget(Paragraph::new(lines.to_vec()), inner),
        CoverDisplay::Image(proto) => frame.render_stateful_widget(
            StatefulImage::<StatefulProtocol>::new().resize(covers::COVER_RESIZE_MODE),
            inner,
            proto,
        ),
        CoverDisplay::Loading => frame.render_widget(
            Paragraph::new(Palette::Muted.span("loading cover…")).centered(),
            inner,
        ),
        CoverDisplay::Unreadable => frame.render_widget(
            Paragraph::new(Palette::Muted.span("cover file unreadable")).centered(),
            inner,
        ),
    }
}

/// Where a cover would be, drawn as a dot-matrix field like the Pocket's
/// screen, with a short line and the keys that add a cover in the middle.
fn draw_cover_placeholder(frame: &mut Frame, area: Rect, what: &str, keys: &[KeyHint]) {
    let dots: Vec<Line> = (0..area.height)
        .map(|y| {
            let row: String = (0..area.width)
                .map(|x| if (x + y) % 2 == 0 { '·' } else { ' ' })
                .collect();
            Line::from(Span::styled(row, Palette::Rule.style()))
        })
        .collect();
    frame.render_widget(Paragraph::new(dots), area);
    let mut lines = vec![Line::from(Palette::Muted.span(what.to_string()))];
    if !keys.is_empty() {
        lines.push(Line::default());
    }
    for (key, what) in keys {
        lines.push(Line::from(vec![
            keycap(key, Palette::Keycap),
            Palette::Muted.span(format!(" {what}")),
        ]));
    }
    let width = lines
        .iter()
        .map(|line| line.width() as u16)
        .max()
        .unwrap_or(0);
    let width = width.min(area.width);
    let height = lines.len() as u16;
    let text = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height: height.min(area.height),
    };
    frame.render_widget(Clear, text);
    frame.render_widget(Paragraph::new(lines), text);
}

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    match app.games.selected_row() {
        Some(GamesTableRow::Game(row)) => {
            if let Some(local) = row.local_entry() {
                lines.push(labeled_text("Library", local.name.clone()));
            }
            if let Some(on_device) = row.device_entry() {
                lines.push(labeled_text("Device", on_device.path.display().to_string()));
            }
            let Files::Single(rom) = row.game().files;
            lines.push(labeled_text("Size", format!("{} bytes", rom.size)));
            lines.push(labeled(
                "crc32",
                Palette::Muted.span(format!("{:08x}", rom.crc32)),
            ));
            lines.push(labeled_text(
                "Save",
                match (row.saves_kept(), row.has_new_save_on_device()) {
                    (0, false) => "none".to_string(),
                    (0, true) => "on the device, not backed up".to_string(),
                    (saves_kept, false) => format!("{saves_kept} kept"),
                    (saves_kept, true) => format!("{saves_kept} kept; a newer one on the device"),
                },
            ));
        }
        Some(GamesTableRow::OrphanSaveInLibrary(orphan)) => {
            lines.push(labeled_text("Saves of", orphan.stem.clone()));
            lines.push(labeled_text("Kept", orphan.save.iter().count().to_string()));
            lines.push(labeled_text("Newest", orphan.save.newest().name.clone()));
            lines.push(Line::default());
            lines.push(Line::from(Palette::Muted.span(
                "The library has no game of this name. A game put there under it has them again.",
            )));
        }
        Some(GamesTableRow::OrphanSaveOnDevice(orphan)) => {
            lines.push(labeled_text("Save", orphan.path.display().to_string()));
            lines.push(labeled_text("Size", format!("{} bytes", orphan.save.size)));
            lines.push(labeled_text("Written", orphan.save.timestamp()));
            lines.push(Line::default());
            lines.push(Line::from(Palette::Muted.span(
                "The device has no game at this path. The Pocket gives it to any game put there.",
            )));
        }
        None => {}
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel_block("DETAILS")),
        area,
    );
}

fn draw_saves(frame: &mut Frame, app: &App, area: Rect, selected_version_index: usize) {
    let versions = app.games.selected_save_versions();
    let on_device = app
        .games
        .selected_row()
        .and_then(GamesTableRow::device_entry);
    let on_device = on_device.and_then(|on_device| on_device.save.clone());
    let block = panel_block(match app.games.selected_row() {
        Some(row) => format!("SAVES OF {}", name_of(row)),
        None => "SAVES".to_string(),
    });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [top, list] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    let device_line = match &on_device {
        Some(save) => {
            let kept = versions
                .iter()
                .any(|version| version.save.has_same_contents_as(save));
            let state = if kept {
                Palette::Good.span("  backed up")
            } else {
                Palette::Warn.span("  not backed up")
            };
            Line::from(vec![
                Palette::Faint.span(format!("{:<DETAILS_LABEL_WIDTH$}", "Device")),
                Span::raw(save.timestamp()),
                state,
            ])
        }
        None => labeled("Device", Palette::Muted.span("no save")),
    };
    frame.render_widget(Paragraph::new(device_line), top);
    if versions.is_empty() {
        frame.render_widget(
            Paragraph::new(Palette::Muted.span("the library has no save of it")),
            list,
        );
        return;
    }
    let rows: Vec<TableRow> = versions
        .iter()
        .enumerate()
        .map(|(i, version)| {
            let is_on_device = on_device
                .as_ref()
                .is_some_and(|device_save| device_save.has_same_contents_as(&version.save));
            let note = match (i, is_on_device) {
                (0, true) => "newest, on the device",
                (0, false) => "newest",
                (_, true) => "on the device",
                _ => "",
            };
            TableRow::new(vec![
                Cell::from(version.name.clone()),
                Cell::from(Text::from(format!("{} bytes", version.save.size)).right_aligned()),
                Cell::from(note).style(Palette::Muted.style()),
            ])
        })
        .collect();
    // The name is what tells versions apart, so it is never cut; the note
    // gives way first in a narrow panel.
    let table = Table::new(
        rows,
        [
            Constraint::Min(21),
            Constraint::Length(12),
            Constraint::Min(0),
        ],
    )
    .header(
        TableRow::new([
            Cell::from("VERSION"),
            Cell::from(Text::from("SIZE").right_aligned()),
            Cell::from(""),
        ])
        .style(Palette::Faint.style()),
    )
    .row_highlight_style(selected_style())
    .highlight_symbol("▸ ");
    let mut state =
        TableState::default().with_selected(Some(selected_version_index.min(versions.len() - 1)));
    frame.render_stateful_widget(table, list, &mut state);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let [status, library_keys, device_keys, general_keys] =
        Layout::vertical([Constraint::Length(1); 4]).areas(area);
    let mut line = match (app.command_status(), app.status_message()) {
        (Some(command_status), _) => {
            let progress = match command_status.target_count {
                0 | 1 => String::new(),
                target_count => format!("  {}/{target_count}", command_status.finished_count),
            };
            let last_target = command_status.last_target.clone().unwrap_or_default();
            let mut line = Line::from(Palette::Ink.span(format!(
                " {} {}{progress}  {last_target}  ",
                spinner_frame(),
                command_status.label
            )));
            if command_status.is_canceling {
                line.push_span(Palette::Muted.span("stopping…"));
            } else {
                line.push_span(keycap("Esc", Palette::Keycap));
                line.push_span(Palette::Muted.span(" stop"));
            }
            line
        }
        (
            None,
            Some(StatusMessage {
                text,
                is_failure: true,
            }),
        ) => Line::from(vec![
            Palette::Bad.span(" › "),
            Palette::Bad.span(text.clone()),
        ]),
        (
            None,
            Some(StatusMessage {
                text,
                is_failure: false,
            }),
        ) => Line::from(vec![
            Palette::Faint.span(" › "),
            Palette::Muted.span(text.clone()),
        ]),
        (None, None) => Line::default(),
    };
    if app.games.marked_count() > 0 {
        line.push_span(Palette::Warn.span(format!("   {} marked", app.games.marked_count())));
    }
    frame.render_widget(Paragraph::new(line), status);
    let [library_line, device_line, general_line] = legend_lines(&legend(app), area.width as usize);
    frame.render_widget(Paragraph::new(library_line), library_keys);
    frame.render_widget(Paragraph::new(device_line), device_keys);
    frame.render_widget(Paragraph::new(general_line), general_keys);
}

/// A key and what it does. [`NO_KEY`] leaves its place in the grid empty.
type KeyHint = (&'static str, &'static str);

const NO_KEY: KeyHint = ("", "");

/// The keys to show, by what they change. They are drawn as a grid so that a
/// key sits above the same key with Shift. Rows don't change with the
/// selection, and a key that doesn't apply says why when pressed.
#[derive(Default)]
struct Legend {
    library: Vec<KeyHint>,
    device: Vec<KeyHint>,
    general: Vec<KeyHint>,
}

fn legend(app: &App) -> Legend {
    if app.games.is_searching() {
        return only_general(vec![("↵", "done"), ("Esc", "clear")]);
    }
    match app.prompt() {
        Some(Prompt::Import(import)) => {
            let enter = match import.target {
                ImportTarget::Bios(_) => "put on the device",
                _ => "import",
            };
            return only_general(vec![
                ("↵", enter),
                ("Esc", "cancel"),
                ("Tab", "complete"),
                ("↑↓", "pick"),
            ]);
        }
        Some(Prompt::Platform(_)) => {
            return only_general(vec![("↵", "import"), ("Esc", "cancel"), ("↑↓", "pick")]);
        }
        Some(Prompt::Rename(_)) => {
            return only_general(vec![("↵", "rename"), ("Esc", "cancel")]);
        }
        None => {}
    }
    let page_keys = vec![("r", "reload"), ("?", "help"), ("q", "quit")];
    match app.current_page() {
        Page::Games if app.games.selected_version_index().is_some() => Legend {
            library: vec![("i", "import"), ("s", "save"), ("x", "trash")],
            device: vec![NO_KEY, ("S", "push"), ("X", "delete")],
            general: vec![("Esc", "back"), ("?", "help"), ("q", "quit")],
        },
        Page::Games => Legend {
            library: vec![
                ("i", "import"),
                ("p", "pull"),
                ("s", "save"),
                ("n", "rename"),
                ("t", "standard"),
                ("x", "trash"),
            ],
            device: vec![
                NO_KEY,
                ("P", "push"),
                NO_KEY,
                ("N", "rename"),
                ("E", "eject"),
                ("X", "delete"),
            ],
            general: vec![
                ("↵", "saves"),
                ("m", "mark"),
                ("d", "DATs"),
                ("r", "reload"),
                ("?", "help"),
                ("q", "quit"),
            ],
        },
        Page::Cores => Legend {
            device: vec![
                ("U", "install or update"),
                ("A", "update all"),
                ("X", "delete"),
            ],
            general: page_keys,
            ..Legend::default()
        },
        Page::Bios => Legend {
            device: vec![("I", "put a file")],
            general: page_keys,
            ..Legend::default()
        },
        Page::Device => Legend {
            general: page_keys,
            ..Legend::default()
        },
    }
}

fn only_general(general: Vec<KeyHint>) -> Legend {
    Legend {
        general,
        ..Legend::default()
    }
}

/// The legend's rows, each with a tag and its keys in columns as wide as their
/// widest key. Columns that don't fit in `width` are left out from the end
/// rather than cut mid-word, and a row with no keys is empty.
fn legend_lines(legend: &Legend, width: usize) -> [Line<'static>; 3] {
    const COLUMN_GAP: usize = 2;
    fn cell_width((key, what): &KeyHint) -> usize {
        match key.is_empty() {
            true => 0,
            false => key.chars().count() + 3 + what.chars().count(),
        }
    }
    let rows = [
        (" LIBRARY ", &legend.library, Palette::KeyLibrary),
        (" DEVICE  ", &legend.device, Palette::KeyDevice),
        (" GENERAL ", &legend.general, Palette::Keycap),
    ];
    let column_count = rows
        .iter()
        .map(|(_, keys, _)| keys.len())
        .max()
        .unwrap_or(0);
    let column_widths: Vec<usize> = (0..column_count)
        .map(|column| {
            let widths = rows.iter().filter_map(|(_, keys, _)| keys.get(column));
            widths.map(cell_width).max().unwrap_or(0)
        })
        .collect();
    rows.map(|(tag, keys, tint)| {
        if keys.is_empty() {
            return Line::default();
        }
        let mut room = width.saturating_sub(tag.len());
        let mut spans = vec![Palette::Faint.span(tag)];
        for (hint, column_width) in keys.iter().zip(&column_widths) {
            if *column_width > room {
                break;
            }
            room -= *column_width;
            let padding = column_width - cell_width(hint);
            if hint.0.is_empty() {
                spans.push(Span::raw(" ".repeat(padding)));
            } else {
                // Removing stands out, whichever side it is on.
                let tint = match hint.0 {
                    "x" | "X" => Palette::KeyRemove,
                    _ => tint,
                };
                spans.push(keycap(hint.0, tint));
                spans.push(Palette::Muted.span(format!(" {}{}", hint.1, " ".repeat(padding))));
            }
            let gap = COLUMN_GAP.min(room);
            room -= gap;
            spans.push(Span::raw(" ".repeat(gap)));
        }
        Line::from(spans)
    })
}

/// A rectangle of at most this size in the middle of `area`.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let (width, height) = (width.min(area.width), height.min(area.height));
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Draws a window of at most this size in the middle of the screen and returns
/// the area inside it.
fn draw_window(frame: &mut Frame, width: u16, height: u16, title: &str, border: Palette) -> Rect {
    let rect = centered_rect(frame.area(), width, height);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border.style())
        .title(Span::styled(
            format!(" {title} "),
            Palette::Ink.style().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    inner
}

fn import_buttons(action: &str) -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![
        Span::styled(format!(" {action} "), selected_style()),
        Span::raw("  "),
        Span::styled(
            " Cancel ",
            Palette::Muted.style().bg(Palette::Keycap.color()),
        ),
    ]))
    .centered()
}

fn draw_import(frame: &mut Frame, import: &ImportPrompt) {
    let (title, action) = match &import.target {
        ImportTarget::Game => ("Import a game or a folder".to_string(), "Import"),
        ImportTarget::Save(local) => (format!("Import a save for {}", local.name), "Import"),
        ImportTarget::Cover(local) => (format!("Import a cover for {}", local.name), "Import"),
        ImportTarget::Bios(row) => {
            let name = row.path.file_name().unwrap_or_default().to_string_lossy();
            (format!("Your {name}, to put on the device"), "Put")
        }
    };
    let inner = draw_window(frame, 64, 16, &title, Palette::Muted);
    let [path, list, buttons] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(path_line(&import.input, path.width as usize)),
        path,
    );
    frame.render_widget(Paragraph::new(match_lines(&import.input, list)), list);
    frame.render_widget(import_buttons(action), buttons);
}

fn draw_rename(frame: &mut Frame, rename: &RenamePrompt) {
    let title = match &rename.target {
        RenameTarget::InLibrary(_) => "Rename in the library",
        RenameTarget::OnDevice(_) => "Rename on the device",
    };
    let inner = draw_window(frame, 76, 5, title, Palette::Muted);
    let [name, buttons] =
        Layout::vertical([Constraint::Length(2), Constraint::Length(1)]).areas(inner);
    let (before, after) = rename.text_around_cursor();
    frame.render_widget(
        Paragraph::new(typed_line(before, &after, name.width as usize)),
        name,
    );
    frame.render_widget(import_buttons("Rename"), buttons);
}

fn draw_platform_prompt(frame: &mut Frame, choice: &PlatformPrompt) {
    let name = choice
        .file_to_import
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let platforms = Platform::iter().count() as u16;
    let title = format!("Which platform is {name} for?");
    let inner = draw_window(frame, 48, platforms + 4, &title, Palette::Muted);
    let [list, buttons] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    // Keeps the one selected in view when the window is cut short.
    let first = (choice.selected_platform_index + 1).saturating_sub(list.height as usize);
    let width = list.width as usize;
    let lines: Vec<Line> = Platform::iter()
        .enumerate()
        .skip(first)
        .map(|(i, platform)| {
            let picked = i == choice.selected_platform_index;
            let style = if picked {
                selected_style()
            } else {
                Palette::Muted.style()
            };
            let mark = if picked { "▸" } else { " " };
            let text = format!(" {mark} {}", platform.id().to_uppercase());
            Line::from(Span::styled(format!("{text:<width$}"), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list);
    frame.render_widget(import_buttons("Import"), buttons);
}

/// ` > path` with the cursor showing. The text before the cursor is cut from
/// the left when it doesn't fit.
fn path_line(input: &PathInput, width: usize) -> Line<'static> {
    let (before, after) = input.text_around_cursor();
    typed_line(before, &after, width)
}

/// ` > text` with the cursor showing. The text before the cursor is cut from
/// the left when it doesn't fit.
fn typed_line(before: String, after: &str, width: usize) -> Line<'static> {
    let room = width.saturating_sub(4);
    let shown = before.chars().count().saturating_sub(room);
    let before: String = before.chars().skip(shown).collect();
    let mut line = Line::from(Palette::Faint.span(" > "));
    line.extend(typed_spans(before, after));
    line
}

/// As many matches as `area` has room for, keeping the one picked in view,
/// and how many more there are.
fn match_lines(input: &PathInput, area: Rect) -> Vec<Line<'static>> {
    let matches = input.matches();
    if matches.is_empty() {
        return vec![Line::from(Palette::Faint.span(" nothing matches"))];
    }
    let height = area.height as usize;
    let room = if matches.len() > height {
        height.saturating_sub(1)
    } else {
        height
    };
    let picked = input.picked_match_index();
    let first = picked.map_or(0, |i| (i + 1).saturating_sub(room));
    let typed = input.typed_name();
    let width = area.width as usize;
    let mut lines: Vec<Line> = matches
        .iter()
        .enumerate()
        .skip(first)
        .take(room)
        .map(|(i, entry)| match_line(entry, typed, picked == Some(i), width))
        .collect();
    let more = matches.len().saturating_sub(first + room);
    if more > 0 {
        lines.push(Line::from(Palette::Faint.span(format!("   … {more} more"))));
    }
    lines
}

/// A match with the typed characters highlighted. The picked one is the
/// selection bar, all the way across `width`.
fn match_line(entry: &PathEntry, typed: &str, picked: bool, width: usize) -> Line<'static> {
    let (base, hit) = if picked {
        (
            selected_style(),
            selected_style().add_modifier(Modifier::UNDERLINED),
        )
    } else {
        (
            Palette::Muted.style(),
            Palette::Ink.style().add_modifier(Modifier::BOLD),
        )
    };
    let name = &entry.name;
    let mut spans = vec![Span::styled(if picked { " ▸ " } else { "   " }, base)];
    let hits = entry.matched_positions(typed);
    for (i, character) in name.chars().enumerate() {
        let style = if hits.contains(&i) { hit } else { base };
        spans.push(Span::styled(character.to_string(), style));
    }
    if entry.is_dir {
        spans.push(Span::styled("/", base));
    }
    let used: usize = spans.iter().map(Span::width).sum();
    spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), base));
    Line::from(spans)
}

fn draw_dialog(frame: &mut Frame, app: &App) {
    let Some(dialog) = app.dialog() else {
        return;
    };
    let longest = dialog.lines.iter().map(|line| line.chars().count()).max();
    let width = longest.unwrap_or(0).max(40) as u16 + 6;
    let height = dialog.lines.len() as u16 + 5;
    let inner = draw_window(frame, width, height, &dialog.title, Palette::Muted);
    let [text, buttons] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let lines: Vec<Line> = dialog
        .lines
        .iter()
        .map(|line| Line::from(Span::raw(format!(" {line}"))))
        .collect();
    frame.render_widget(Paragraph::new(lines), text);
    let mut spans = Vec::new();
    for (i, button) in dialog.buttons.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        // A button that removes something is red, like the keys that open it.
        let style = match (i == dialog.focused_button, button.is_removal) {
            (true, false) => selected_style(),
            (true, true) => selected_style().bg(Palette::Bad.color()),
            (false, false) => Palette::Muted.style().bg(Palette::Keycap.color()),
            (false, true) => Palette::Ink.style().bg(Palette::KeyRemove.color()),
        };
        spans.push(Span::styled(format!(" {} ", button.label), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).centered(), buttons);
}
