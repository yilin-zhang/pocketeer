//! The three tabs that only exist with a card: Cores (installed cores and
//! the inventory in one table), BIOS (every file the installed cores declare)
//! and Card (status, junk and the recent log).

use super::*;

pub(super) fn draw_cores(frame: &mut Frame, app: &mut App, area: Rect) {
    if !card_tab(frame, app, area) {
        return;
    }
    let (filter_row, table_area, right) = split_tab(app, area);
    let snap = app.card_view().expect("checked above");
    let inventory_known = snap.inventory.is_some();
    let rows: Vec<Row> = app
        .cores
        .rows
        .iter()
        .map(|r| {
            let installed = match &r.installed {
                Some(v) => Cell::from(v.clone()).style(Style::new().fg(GOOD)),
                None => Cell::from(""),
            };
            let (status, style) = match (&r.installed, &r.latest) {
                (Some(_), Some(l)) if r.update => (format!("→ {l}"), Style::new().fg(WARN)),
                (Some(_), Some(_)) => ("up to date".to_string(), Style::new().fg(GOOD)),
                (Some(_), None) if r.in_inventory => ("no release".to_string(), Style::new().dim()),
                (Some(_), None) if inventory_known => {
                    ("not in inventory".to_string(), Style::new().dim())
                }
                (Some(_), None) => ("?".to_string(), Style::new().dim()),
                (None, Some(l)) => (l.clone(), Style::new()),
                (None, None) => ("-".to_string(), Style::new().dim()),
            };
            let files = if r.missing_files > 0 {
                Cell::from(format!("{} missing", r.missing_files)).style(Style::new().fg(BAD))
            } else {
                Cell::from("")
            };
            let license = if r.license {
                Cell::from("license").style(Style::new().fg(WARN))
            } else {
                Cell::from("")
            };
            Row::new(vec![
                Cell::from(r.id.clone()),
                Cell::from(r.platforms.join(", ")).style(Style::new().dim()),
                installed,
                Cell::from(status).style(style),
                files,
                license,
            ])
        })
        .collect();
    let title = format!(
        "CORES  ·  {} installed  ·  {}",
        snap.cores.cores.len(),
        snap.inventory
            .as_ref()
            .map(|i| format!("{} in inventory", i.cores.len()))
            .unwrap_or_else(|| "inventory offline".into())
    );
    let table = styled_table(
        rows,
        [
            Constraint::Min(24),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(16),
            Constraint::Length(10),
            Constraint::Length(8),
        ],
        ["CORE", "PLATFORMS", "INSTALLED", "LATEST", "BIOS", ""],
        title,
        table_area.width,
    );
    let sel = (!app.cores.rows.is_empty()).then_some(app.cores.sel);
    draw_filter_bar(frame, app, filter_row);
    render_table(frame, app, table_area, table, sel);

    let snap = app.card_view().expect("checked above");
    let mut lines: Vec<Line> = Vec::new();
    if let Some(c) = app.selected_core() {
        lines.push(Line::from(Span::styled(c.id.clone(), Style::new().bold())));
        lines.push(kv_text("Platforms", c.platforms.join(", ")));
        match &c.installed {
            Some(v) => lines.push(kv_text("Installed", v.clone())),
            None => lines.push(kv("Installed", dim("no  (Enter installs it)"))),
        }
        match &c.latest {
            Some(l) => {
                let date = c
                    .date
                    .as_deref()
                    .map(|d| format!("  ({d})"))
                    .unwrap_or_default();
                lines.push(kv_text("Latest", format!("{l}{date}")));
            }
            None if c.in_inventory => lines.push(kv_text("Latest", "no release published")),
            None => lines.push(kv("Inventory", dim("not listed"))),
        }
        if c.license {
            lines.push(kv(
                "License",
                fg("this core needs a licensed file; see its repo", WARN),
            ));
        }
        if let Some(repo) = &c.repo {
            lines.push(kv_text("Repo", repo.clone()));
        }
        if let Some(d) = &c.description {
            lines.push(Line::from(""));
            lines.push(Line::from(d.clone()));
        }
        let missing = app.bios_for(&c.id);
        if !missing.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Missing files",
                Style::new().bold().fg(BAD),
            )));
            for f in missing {
                let note = match &f.status {
                    FileStatus::WrongHash { .. } => " (wrong md5)",
                    FileStatus::FoundAtRoot { .. } => " (at root → BIOS tab, p)",
                    FileStatus::FoundAtRootZipped { .. } => " (zipped at root)",
                    _ => "",
                };
                lines.push(Line::from(format!("  {}{note}", f.path.display())));
            }
        }
    } else if snap.inventory.is_none() && app.cores.rows.is_empty() {
        lines.push(
            Line::from("Inventory unavailable (offline and nothing cached?). Press r to retry.")
                .fg(BAD),
        );
    } else {
        lines.push(Line::from("No cores match.").dim());
    }
    if !snap.cores.broken.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(fg(
            format!("{} broken core folder(s):", snap.cores.broken.len()),
            BAD,
        )));
        for b in &snap.cores.broken {
            lines.push(Line::from(format!("  {}  [{}]", b.id, b.error.code)));
        }
    }
    details(frame, right, lines);
}

// ---------------------------------------------------------------------------

fn status_cell(f: &FileStatus) -> (String, Style) {
    match f {
        FileStatus::Present => ("present".into(), Style::new().fg(GOOD)),
        FileStatus::Missing => ("missing".into(), Style::new().fg(BAD)),
        FileStatus::WrongHash { .. } => ("wrong md5".into(), Style::new().fg(WARN)),
        FileStatus::Unreadable { .. } => ("unreadable".into(), Style::new().fg(WARN)),
        FileStatus::FoundAtRoot { .. } => ("at card root → p".into(), Style::new().fg(ACCENT)),
        FileStatus::FoundAtRootZipped { .. } => ("zipped at root".into(), Style::new().fg(ACCENT)),
    }
}

pub(super) fn draw_bios(frame: &mut Frame, app: &mut App, area: Rect) {
    if !card_tab(frame, app, area) {
        return;
    }
    let (filter_row, table_area, right) = split_tab(app, area);
    let snap = app.card_view().expect("checked above");
    let rows: Vec<Row> = app
        .bios
        .rows
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
    let title = format!(
        "REQUIRED FILES  ·  {} present  ·  {} missing  ·  {} wrong  ·  {} placeable",
        s.present, s.missing, s.wrong_hash, s.placeable
    );
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
    let sel = (!app.bios.rows.is_empty()).then_some(app.bios.sel);
    draw_filter_bar(frame, app, filter_row);
    render_table(frame, app, table_area, table, sel);

    let snap = app.card_view().expect("checked above");
    let mut lines: Vec<Line> = Vec::new();
    match app.selected_bios() {
        Some((core, f)) => {
            lines.push(kv_text("Core", core.core_id.clone()));
            lines.push(kv_text("Slot", f.slot.clone()));
            lines.push(kv_text("Path", f.path.display().to_string()));
            lines.push(kv_text(
                "Need",
                if f.required {
                    "required: the core won't run without it"
                } else {
                    "optional"
                },
            ));
            if let Some(md5) = &f.expected_md5 {
                lines.push(kv_text("md5", md5.clone()));
            }
            let (status, style) = status_cell(&f.status);
            lines.push(kv("Status", Span::styled(status, style)));
            lines.push(Line::from(""));
            let advice: Vec<String> = match &f.status {
                FileStatus::Present => vec!["Nothing to do.".into()],
                FileStatus::Missing => vec![
                    "pocketeer never downloads BIOS files. Copy yours to the root of the card \
                     with that exact file name, then press p to move it into place."
                        .into(),
                ],
                FileStatus::WrongHash { actual_md5 } => vec![
                    format!("The file on the card has md5 {actual_md5}."),
                    "The core expects a different dump; replace the file.".into(),
                ],
                FileStatus::Unreadable { error } => vec![format!(
                    "The file is on the card but could not be read: {}",
                    error.message
                )],
                FileStatus::FoundAtRoot { root_file } => vec![format!(
                    "{} is at the card root. Press p to move it into place.",
                    root_file.display()
                )],
                FileStatus::FoundAtRootZipped { zip, inner } => vec![format!(
                    "{inner} is inside {} at the card root; unzip it first, then p.",
                    zip.display()
                )],
            };
            for a in advice {
                lines.push(Line::from(a).dim());
            }
            for n in &core.notes {
                lines.push(Line::from(format!("note: {n}")).dim());
            }
        }
        None if app.bios.filter == BiosFilter::Problems && !snap.bios.cores.is_empty() => {
            lines.push(Line::from("Every core has the files it needs.").fg(GOOD));
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
    let [left, right] = split_cols(app, area);
    let [card_area, device_area] =
        Layout::vertical([Constraint::Length(7), Constraint::Min(5)]).areas(left);
    let [junk_area, log_area] = split_rows(app, right, Splitter::DeviceRows);
    let snap = app.card_view().expect("checked above");

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

    let actions = vec![
        Line::from(format!("{} ROM file(s) on this card", snap.card_roms.len())),
        Line::from(""),
        key_line(&[("3", "manage ROMs on the Games tab")], usize::MAX),
        key_line(&[("V", "review save backups")], usize::MAX),
    ];
    frame.render_widget(
        Paragraph::new(actions)
            .wrap(Wrap { trim: false })
            .block(panel("MANAGE")),
        device_area,
    );

    let junk = vec![
        Line::from("Scanned only when requested.").dim(),
        key_line(&[("c", "scan and review cleanup")], usize::MAX),
    ];
    frame.render_widget(Paragraph::new(junk).block(panel("MACOS JUNK")), junk_area);

    let room = log_area.height.saturating_sub(2) as usize;
    let mut recent: Vec<Line> = app
        .log
        .iter()
        .rev()
        .take(room)
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                Line::from(Span::styled(l.clone(), Style::new().fg(INK)))
            } else {
                Line::from(dim(l.clone()))
            }
        })
        .collect();
    if recent.is_empty() {
        recent.push(Line::from(faint("nothing yet")));
    }
    frame.render_widget(Paragraph::new(recent).block(panel("RECENT")), log_area);
}
