//! The terminal UI and its event loop.

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
use covers::CoverLoader;
use path_input::DirReader;
use worker::{Worker, WorkerMessage};

/// What the loop wakes up for.
enum Event {
    Key(KeyEvent),
    Resized,
    Worker(Box<WorkerMessage>),
    CoverReady,
    /// A folder to offer paths from has been read.
    DirRead,
}

/// How often the spinner moves while something is running.
const SPINNER_INTERVAL: Duration = Duration::from_millis(150);

pub fn run(library: PathBuf, device: Option<PathBuf>) -> ExitCode {
    // Ask the terminal which image protocol it speaks before entering the
    // alternate screen. POCKETEER_NO_IMAGES=1 skips the query.
    let no_images = std::env::var_os("POCKETEER_NO_IMAGES").is_some_and(|value| !value.is_empty());
    let picker = if no_images {
        Picker::halfblocks()
    } else {
        Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
    };
    let mut terminal = ratatui::init();
    let (mut app, rx) = start_threads(library, device, picker);
    let result = run_event_loop(&mut terminal, &mut app, &rx);
    ratatui::restore();
    // Dropping `app` waits for the worker, after the terminal is restored.
    if app.is_busy() {
        eprintln!("finishing what is under way; Ctrl-C stops it now");
    }
    match result {
        Ok(None) => ExitCode::SUCCESS,
        Ok(Some(reason)) => {
            eprintln!("error: couldn't open the library: {reason}");
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("error: terminal: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Starts the threads and the app. The threads report through the returned
/// receiver.
fn start_threads(
    library: PathBuf,
    device: Option<PathBuf>,
    picker: Picker,
) -> (App, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let input = tx.clone();
    thread::spawn(move || read_input(&input));
    let to_ui = tx.clone();
    let worker = Worker::spawn(library, device, move |message| {
        let _ = to_ui.send(Event::Worker(Box::new(message)));
    });
    let to_ui = tx.clone();
    let cover_loader = CoverLoader::spawn(picker, move || {
        let _ = to_ui.send(Event::CoverReady);
    });
    let paths = DirReader::spawn(move || {
        let _ = tx.send(Event::DirRead);
    });
    (App::new(worker, cover_loader, paths), rx)
}

/// Runs until the user quits, or returns the reason if the library can't be
/// opened.
fn run_event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &mpsc::Receiver<Event>,
) -> io::Result<Option<String>> {
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        // Only a running command has anything to animate.
        let next = if app.is_busy() {
            rx.recv_timeout(SPINNER_INTERVAL)
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
                Event::Worker(message) => match *message {
                    WorkerMessage::LibraryOpenFailed(reason) => return Ok(Some(reason)),
                    message => app.handle_worker_message(message),
                },
                Event::CoverReady => app.on_cover_ready(),
                Event::DirRead => app.on_dir_read(),
                Event::Resized => {}
            }
        }
        if app.should_quit() {
            return Ok(None);
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
