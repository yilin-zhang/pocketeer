//! The terminal UI: this runs the loop, `worker` holds the `Pocketeer` on its
//! own thread, `covers` prepares covers on another, `path_input` reads folders
//! on a third, `app` keeps the screen's state and takes keys, and `ui` draws.

mod app;
mod covers;
mod path_input;
mod ui;
mod worker;

use std::{
    io,
    path::PathBuf,
    process::ExitCode,
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::Duration,
};

use crossterm::event::{self, Event as TerminalEvent, KeyEvent, KeyEventKind};
use ratatui_image::picker::Picker;

use app::App;
use covers::Covers;
use path_input::PathReader;
use worker::{Update, Worker};

/// What the loop wakes up for.
enum Event {
    Key(KeyEvent),
    /// The terminal changed size.
    Resized,
    Worker(Box<Update>),
    CoverReady,
    /// A folder to offer paths from has been read.
    FolderRead,
}

/// How often the spinner moves while something is running.
const SPIN: Duration = Duration::from_millis(150);

pub fn run(library: PathBuf, device: Option<PathBuf>) -> ExitCode {
    // Ask the terminal which image protocol it speaks before entering the
    // alternate screen. POCKETEER_NO_IMAGES=1 skips the query.
    let no_images = std::env::var_os("POCKETEER_NO_IMAGES").is_some_and(|v| !v.is_empty());
    let picker = if no_images {
        Picker::halfblocks()
    } else {
        Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
    };
    let mut terminal = ratatui::init();
    let (mut app, rx) = start_threads(library, device, picker);
    let result = run_event_loop(&mut terminal, &mut app, &rx);
    ratatui::restore();
    // `app` going waits for the worker, with the terminal the user's again.
    if app.is_busy() {
        eprintln!("finishing what is under way; Ctrl-C stops it now");
    }
    match result {
        Ok(None) => ExitCode::SUCCESS,
        Ok(Some(reason)) => {
            eprintln!("error: couldn't open the library: {reason}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("error: terminal: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The threads, and the app that hears from them through what is returned.
fn start_threads(
    library: PathBuf,
    device: Option<PathBuf>,
    picker: Picker,
) -> (App, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let input = tx.clone();
    thread::spawn(move || read_input(&input));
    let to_ui = tx.clone();
    let worker = Worker::spawn(library, device, move |update| {
        let _ = to_ui.send(Event::Worker(Box::new(update)));
    });
    let to_ui = tx.clone();
    let covers = Covers::spawn(picker, move || {
        let _ = to_ui.send(Event::CoverReady);
    });
    let paths = PathReader::spawn(move || {
        let _ = tx.send(Event::FolderRead);
    });
    (App::new(worker, covers, paths), rx)
}

/// Runs until the user quits, or the library can't be opened, for the reason
/// returned.
fn run_event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &mpsc::Receiver<Event>,
) -> io::Result<Option<String>> {
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        // Only a running job has anything to animate.
        let next = if app.is_busy() {
            rx.recv_timeout(SPIN)
        } else {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        let first = match next {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        };
        // Everything already waiting goes before the next draw, so holding
        // an arrow key doesn't redraw for each repeat.
        for event in first.into_iter().chain(rx.try_iter()) {
            match event {
                Event::Key(key) => app.handle_key(key),
                Event::Worker(update) => app.update(*update),
                Event::CoverReady => app.on_cover_ready(),
                Event::FolderRead => app.on_folder_read(),
                Event::Resized => {}
            }
        }
        if app.should_quit() {
            return Ok(app.take_library_failure());
        }
    }
}

fn read_input(tx: &mpsc::Sender<Event>) {
    loop {
        let event = match event::read() {
            Ok(TerminalEvent::Key(key)) if key.kind == KeyEventKind::Press => Event::Key(key),
            Ok(TerminalEvent::Resize(..)) => Event::Resized,
            Ok(_) => continue,
            Err(_) => return,
        };
        if tx.send(event).is_err() {
            return;
        }
    }
}
