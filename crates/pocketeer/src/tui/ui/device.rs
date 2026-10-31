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

    let mut dev: Vec<Line> = Vec::new();
    match app.library_view().and_then(|l| l.device.as_ref()) {
        Some(d) => {
            dev.push(kv_text("Name", format!("{}  ({})", d.name, d.id)));
            dev.push(kv_text(
                "Platforms",
                if d.platforms.is_empty() {
                    "(none)".to_string()
                } else {
                    d.platforms.join(", ")
                },
            ));
            dev.push(kv_text(
                "Cores",
                if d.cores.is_empty() {
                    "(none)".to_string()
                } else {
                    d.cores.join(", ")
                },
            ));
            dev.push(kv_text(
                "Last sync",
                match d.last_sync {
                    Some(t) => human::time_ago(t),
                    None => "never".into(),
                },
            ));
            dev.push(Line::from(""));
            dev.push(key_line(&[("D", "change subscriptions")]));
            dev.push(key_line(&[("S", "sync ROMs and cores from the library")]));
            dev.push(key_line(&[("V", "sync saves both ways, newest wins")]));
        }
        None if app.library_view().is_some() => {
            dev.push(Line::from("This card isn't registered as a device yet."));
            dev.push(Line::from(""));
            dev.push(key_line(&[(
                "D",
                "give it a name and choose what it syncs",
            )]));
        }
        None => {
            dev.push(Line::from("No library yet."));
            dev.push(Line::from(""));
            dev.push(key_line(&[("n", "on the Games tab creates one")]));
            dev.push(key_line(&[("D", "then registers this card")]));
        }
    }
    frame.render_widget(
        Paragraph::new(dev)
            .wrap(Wrap { trim: false })
            .block(panel("LIBRARY DEVICE")),
        device_area,
    );

    let mut junk: Vec<Line> = Vec::new();
    if snap.clean.entries.is_empty() {
        junk.push(Line::from(Span::styled("none", Style::new().fg(GOOD))));
    } else {
        junk.push(Line::from(format!(
            "{} item(s), {}",
            snap.clean.entries.len(),
            human::bytes(snap.clean.bytes)
        )));
        let room = junk_area.height.saturating_sub(4) as usize;
        for e in snap.clean.entries.iter().take(room) {
            junk.push(Line::from(format!("  {}", e.path.display())).dim());
        }
        if snap.clean.entries.len() > room {
            junk.push(
                Line::from(format!("  … and {} more", snap.clean.entries.len() - room)).dim(),
            );
        }
        junk.push(key_line(&[("c", "delete them")]));
    }
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
