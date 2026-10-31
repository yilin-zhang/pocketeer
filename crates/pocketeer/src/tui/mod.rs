//! Terminal UI. One worker thread runs core operations; the UI thread draws
//! and handles keys. They talk over channels (`jobs.rs`).

mod app;
mod jobs;
mod ui;

use std::{
    process::ExitCode,
    time::{Duration, Instant},
};

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};

use crate::cli::Global;

pub fn run(global: &Global) -> ExitCode {
    // Ask the terminal which image protocol it speaks (kitty, iTerm2, sixel)
    // before entering the alternate screen; fall back to half-block cells.
    // POCKETEER_NO_IMAGES=1 skips the query (odd terminals, tests, screen readers).
    let picker = if std::env::var_os("POCKETEER_NO_IMAGES").is_some_and(|v| !v.is_empty()) {
        ratatui_image::picker::Picker::halfblocks()
    } else {
        ratatui_image::picker::Picker::from_query_stdio()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks())
    };
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let mut app = app::App::new(global.clone(), picker);
    app.reload();
    let mut last_tick = Instant::now();

    let result = loop {
        if let Err(e) = terminal.draw(|frame| ui::draw(frame, &mut app)) {
            break Err(e);
        }
        // Drain worker messages, then wait briefly for a key. Every key that
        // is already queued is handled before the next draw, so holding an
        // arrow key doesn't redraw once per repeat.
        app.pump();
        if last_tick.elapsed() >= Duration::from_secs(2) {
            last_tick = Instant::now();
            app.tick();
        }
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => {
                let mut failed = None;
                loop {
                    match event::read() {
                        Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => app.key(key),
                        Ok(Event::Mouse(m)) => app.mouse(m),
                        Ok(_) => {}
                        Err(e) => {
                            failed = Some(e);
                            break;
                        }
                    }
                    if app.should_quit || !matches!(event::poll(Duration::ZERO), Ok(true)) {
                        break;
                    }
                }
                if let Some(e) = failed {
                    break Err(e);
                }
            }
            Ok(false) => {}
            Err(e) => break Err(e),
        }
        if app.should_quit {
            break Ok(());
        }
    };

    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: terminal: {e}");
            ExitCode::FAILURE
        }
    }
}
