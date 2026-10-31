//! The Cores tab: installed cores and the inventory in one table.

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
