//! The three tabs that only exist with a card: Cores (installed cores and
//! the inventory in one table), BIOS (every file the installed cores declare)
//! and Card (status, junk and the recent log).

use super::*;
use crate::text::Label;

pub(super) fn draw_cores(frame: &mut Frame, app: &mut App, area: Rect) {
    if !card_tab(frame, app, area) {
        return;
    }
    let (filter_row, table_area, right) = split_tab(app, area);
    let snap = app.session().card().expect("checked above");
    let inventory_known = snap.inventory.is_some();
    // Every column but the id is as wide as its widest cell, and the id
    // takes the rest. Platform lists and a few odd version strings run long,
    // so those two columns stop at 12 (the details pane has them in full).
    let cells: Vec<[(String, Style); 6]> = app
        .cores()
        .pane()
        .rows()
        .iter()
        .map(|r| {
            let installed = match &r.installed {
                Some(i) => (i.version.clone(), Palette::Good.style()),
                None => (String::new(), Style::new()),
            };
            let latest = r.latest.as_ref().map(|l| &l.version);
            let status = match (&r.installed, latest) {
                (Some(_), Some(l)) if r.update_available => {
                    (format!("→ {l}"), Palette::Warn.style())
                }
                (Some(_), Some(_)) => ("✓".to_string(), Palette::Good.style()),
                (Some(_), None) if r.in_inventory => ("no release".to_string(), Style::new().dim()),
                (Some(_), None) if inventory_known => {
                    ("not in inventory".to_string(), Style::new().dim())
                }
                (Some(_), None) => ("?".to_string(), Style::new().dim()),
                (None, Some(l)) => (l.clone(), Style::new()),
                (None, None) => ("-".to_string(), Style::new().dim()),
            };
            let missing = snap.missing_files(&r.id).len();
            let files = match missing {
                0 => (String::new(), Style::new()),
                n => (format!("{n} missing"), Palette::Bad.style()),
            };
            let license = match r.latest.as_ref().is_some_and(|l| l.requires_license) {
                true => ("license".to_string(), Palette::Warn.style()),
                false => (String::new(), Style::new()),
            };
            [
                (r.id.clone(), Style::new()),
                (r.platform_ids.join(", "), Style::new().dim()),
                installed,
                status,
                files,
                license,
            ]
        })
        .collect();
    let header = ["CORE", "PLATFORMS", "INSTALLED", "LATEST", "BIOS", ""];
    let width = |col: usize| {
        let widest = cells.iter().map(|c| c[col].0.chars().count()).max();
        widest.unwrap_or(0).max(header[col].len()) as u16
    };
    let widths = [
        Constraint::Fill(1),
        Constraint::Length(width(1).min(12)),
        Constraint::Length(width(2)),
        Constraint::Length(width(3).min(12)),
        Constraint::Length(width(4)),
        Constraint::Length(width(5)),
    ];
    let rows: Vec<Row> = cells
        .into_iter()
        .map(|c| Row::new(c.map(|(text, style)| Cell::from(text).style(style))))
        .collect();
    let title = format!(
        "CORES  ·  {}",
        snap.inventory
            .as_ref()
            .map(|i| format!("{} in inventory", i.cores.len()))
            .unwrap_or_else(|| "inventory offline".into())
    );
    let table = styled_table(rows, widths, header, title, table_area.width);
    let pane = app.cores().pane();
    let sel = (!pane.rows().is_empty()).then_some(pane.sel());
    draw_filter_bar(frame, app, filter_row);
    render_table(frame, app, table_area, table, sel);

    let snap = app.session().card().expect("checked above");
    let mut lines: Vec<Line> = Vec::new();
    if let Some(c) = app.cores().pane().selected() {
        // The table has the platforms and versions; this adds the rest.
        lines.push(Line::from(Span::styled(c.id.clone(), Style::new().bold())));
        match &c.latest {
            Some(l) => {
                let date = l
                    .date_release
                    .as_deref()
                    .map(|d| format!("  ({d})"))
                    .unwrap_or_default();
                lines.push(kv_text("Latest", format!("{}{date}", l.version)));
            }
            None if c.in_inventory => lines.push(kv_text("Latest", "no release published")),
            None => lines.push(kv("Inventory", Palette::Muted.span("not listed"))),
        }
        if c.latest.as_ref().is_some_and(|l| l.requires_license) {
            lines.push(kv(
                "License",
                Palette::Warn.span("this core needs a licensed file; see its repo"),
            ));
        }
        if let Some(repo) = &c.repo_url {
            lines.push(kv_text("Repo", repo.clone()));
        }
        if let Some(d) = c.installed.as_ref().and_then(|i| i.description.as_ref()) {
            lines.push(Line::from(""));
            lines.push(Line::from(d.clone()));
        }
        let missing = snap.missing_files(&c.id);
        if !missing.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Missing files",
                Style::new().bold().fg(Palette::Bad.color()),
            )));
            for f in missing {
                let note = match &f.status {
                    FileStatus::WrongHash { .. } => " (wrong md5)",
                    FileStatus::AtRoot {
                        found: RootFile::File { .. },
                    } => " (at root → BIOS tab, p)",
                    FileStatus::AtRoot {
                        found: RootFile::Zip { .. },
                    } => " (zipped at root)",
                    _ => "",
                };
                lines.push(Line::from(format!("  {}{note}", f.path.display())));
            }
        }
    } else if snap.inventory.is_none() && app.cores().pane().rows().is_empty() {
        lines.push(
            Line::from("Inventory unavailable (offline and nothing cached?). Press r to retry.")
                .fg(Palette::Bad.color()),
        );
    } else {
        lines.push(Line::from("No cores match.").dim());
    }
    if !snap.cores.broken.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Palette::Bad.span(format!(
            "{} broken core folder(s):",
            snap.cores.broken.len()
        ))));
        for b in &snap.cores.broken {
            lines.push(Line::from(format!("  {}  [{}]", b.id, b.error.code)));
        }
    }
    details(frame, right, lines);
}

// ---------------------------------------------------------------------------

fn status_cell(f: &FileStatus) -> (String, Style) {
    match f {
        FileStatus::Present => (f.label().into(), Palette::Good.style()),
        FileStatus::Missing => (f.label().into(), Palette::Bad.style()),
        FileStatus::WrongHash { .. } | FileStatus::Unreadable { .. } => {
            (f.label().into(), Palette::Warn.style())
        }
        // `p` places it; a key is listed where it acts.
        FileStatus::AtRoot {
            found: RootFile::File { .. },
        } => (format!("{} → p", f.label()), Palette::Ink.style()),
        FileStatus::AtRoot {
            found: RootFile::Zip { .. },
        } => (f.label().into(), Palette::Ink.style()),
    }
}

pub(super) fn draw_bios(frame: &mut Frame, app: &mut App, area: Rect) {
    if !card_tab(frame, app, area) {
        return;
    }
    let (filter_row, table_area, right) = split_tab(app, area);
    let snap = app.session().card().expect("checked above");
    let rows: Vec<Row> = app
        .bios()
        .pane()
        .rows()
        .iter()
        .filter_map(|(ci, fi)| {
            let core = snap.bios.cores.get(*ci)?;
            let f = core.files.get(*fi)?;
            let (status, style) = status_cell(&f.status);
            Some(Row::new(vec![
                Cell::from(core.core_id.clone()),
                Cell::from(
                    f.path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                ),
                Cell::from(status).style(style),
                Cell::from(if f.required { "required" } else { "optional" })
                    .style(Style::new().dim()),
            ]))
        })
        .collect();
    let s = &snap.bios.summary;
    let mut title = "REQUIRED FILES".to_string();
    for (n, what) in [
        (s.missing, "missing"),
        (s.wrong_hash, "wrong"),
        (s.placeable, "at the card root"),
    ] {
        if n > 0 {
            title.push_str(&format!("  ·  {n} {what}"));
        }
    }
    let table = styled_table(
        rows,
        [
            Constraint::Length(24),
            Constraint::Min(20),
            Constraint::Length(18),
            Constraint::Length(9),
        ],
        ["CORE", "FILE", "STATUS", "NEED"],
        title,
        table_area.width,
    );
    let pane = app.bios().pane();
    let sel = (!pane.rows().is_empty()).then_some(pane.sel());
    draw_filter_bar(frame, app, filter_row);
    render_table(frame, app, table_area, table, sel);

    let snap = app.session().card().expect("checked above");
    let mut lines: Vec<Line> = Vec::new();
    match app.bios().selected(app.session()) {
        Some((core, f)) => {
            lines.push(kv_text("Slot", f.slot.clone()));
            lines.push(kv_text("Path", f.path.display().to_string()));
            if let Some(md5) = &f.expected_md5 {
                lines.push(kv_text("md5", md5.clone()));
            }
            lines.push(Line::from(""));
            let advice: Vec<String> = match &f.status {
                FileStatus::Present => Vec::new(),
                FileStatus::Missing => {
                    vec!["Copy it to the card root under this name, then press p.".into()]
                }
                FileStatus::WrongHash { actual_md5 } => vec![
                    format!("The file on the card has md5 {actual_md5}."),
                    "The core expects a different dump; replace the file.".into(),
                ],
                FileStatus::Unreadable { error } => vec![format!(
                    "The file is on the card but could not be read: {}",
                    error.message
                )],
                FileStatus::AtRoot {
                    found: RootFile::File { path: root_file },
                } => vec![format!(
                    "{} is at the card root. Press p to move it into place.",
                    root_file.display()
                )],
                FileStatus::AtRoot {
                    found: RootFile::Zip { zip, inner },
                } => vec![format!(
                    "{inner} is inside {} at the card root; unzip it first, then p.",
                    zip.display()
                )],
            };
            for a in advice {
                lines.push(Line::from(a).dim());
            }
            for n in &core.notes {
                lines.push(Line::from(format!("note: {}", text::card::note(n))).dim());
            }
        }
        None if app.bios().pane().filter() == BiosFilter::Problems
            && !snap.bios.cores.is_empty() =>
        {
            lines.push(Line::from("Every core has the files it needs.").fg(Palette::Good.color()));
        }
        None if snap.bios.cores.is_empty() => {
            lines.push(Line::from("No installed core declares extra files.").dim());
        }
        None => lines.push(Line::from("No required files match.").dim()),
    }
    details(frame, right, lines);
}

// ---------------------------------------------------------------------------

pub(super) fn draw_device(frame: &mut Frame, app: &mut App, area: Rect) {
    if !card_tab(frame, app, area) {
        return;
    }
    let [card_area, log_area] = split_cols(app, area);
    let snap = app.session().card().expect("checked above");

    let st = &snap.status;
    let mut card = vec![kv_text("Root", tilde(&st.root))];
    if let Some(fw) = &st.firmware {
        card.push(kv_text(
            "Firmware",
            format!(
                "{}{}",
                fw.version.as_deref().unwrap_or("unknown"),
                fw.build_date
                    .as_deref()
                    .map(|d| format!("  (built {d})"))
                    .unwrap_or_default()
            ),
        ));
    }
    if let Some(d) = st.disk {
        card.push(kv_text(
            "Disk",
            format!(
                "{} free of {}",
                text::bytes(d.available_bytes),
                text::bytes(d.total_bytes)
            ),
        ));
    }
    card.push(kv_text("Cores", st.installed_cores.to_string()));
    card.push(kv_text("ROMs", snap.files.len().to_string()));
    card.push(kv_text(
        "Inventory",
        match snap.inventory.as_ref().map(|i| &i.source) {
            Some(pocketeer_core::net::Source::Network) => "fetched just now".to_string(),
            Some(pocketeer_core::net::Source::Cache { age_secs, .. }) => {
                format!("from cache, {} old", text::duration(*age_secs))
            }
            None => "unavailable".to_string(),
        },
    ));
    frame.render_widget(Paragraph::new(card).block(panel("CARD")), card_area);

    let room = log_area.height.saturating_sub(2) as usize;
    let mut recent: Vec<Line> = app
        .session()
        .log()
        .take(room)
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                Line::from(Span::styled(l.to_string(), Palette::Ink.style()))
            } else {
                Line::from(Palette::Muted.span(l))
            }
        })
        .collect();
    if recent.is_empty() {
        recent.push(Line::from(Palette::Faint.span("nothing yet")));
    }
    frame.render_widget(Paragraph::new(recent).block(panel("RECENT")), log_area);
}
