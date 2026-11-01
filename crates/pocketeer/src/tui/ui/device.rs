//! The Device tab: card, registered device, junk and recent log.

use super::*;

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
                human::bytes(d.available_bytes),
                human::bytes(d.total_bytes)
            ),
        ));
    }
    card.push(kv_text("Cores", st.installed_cores.to_string()));
    card.push(kv_text(
        "Inventory",
        match snap.inventory.as_ref().map(|i| &i.source) {
            Some(pocketeer_core::net::Source::Network) => "fetched just now".to_string(),
            Some(pocketeer_core::net::Source::Cache { age_secs, .. }) => {
                format!("from cache, {} old", human::duration(*age_secs))
            }
            None => "unavailable".to_string(),
        },
    ));
    frame.render_widget(Paragraph::new(card).block(panel("CARD")), card_area);

    let actions = vec![
        Line::from(format!("{} ROM file(s) on this card", snap.card_roms.len())),
        Line::from(""),
        key_line(&[("3", "manage ROMs on the Games tab")]),
        key_line(&[("V", "review save backups")]),
    ];
    frame.render_widget(
        Paragraph::new(actions)
            .wrap(Wrap { trim: false })
            .block(panel("MANAGE")),
        device_area,
    );

    let junk = vec![
        Line::from("Scanned only when requested.").dim(),
        key_line(&[("c", "scan and review cleanup")]),
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
