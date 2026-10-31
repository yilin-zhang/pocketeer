//! The BIOS tab: every file the installed cores declare, with its status.

use super::*;

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
