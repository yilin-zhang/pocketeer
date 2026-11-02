//! The one place that decides how results reach the user.
//!
//! JSON mode: exactly one envelope object on stdout, nothing else.
//! Human mode: data on stdout, errors and progress on stderr.

use std::{
    io::{IsTerminal, Write},
    process::ExitCode,
};

use pocketeer_core::{Error, ErrorInfo, plan::Event};
use serde::Serialize;

use crate::text::{Reader, Text};

/// Bump when a released JSON shape changes incompatibly.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
struct Envelope<'a, T: Serialize> {
    schema_version: u32,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<&'a T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorInfo>,
}

/// A command result that may carry data *and* a failure (e.g. a plan that
/// needs confirmation, or a batch where some items failed). In that case the
/// envelope has both `data` and `error`, and the exit code is 1.
pub trait Outcome: Serialize {
    fn error(&self) -> Option<Error> {
        None
    }
}

// Reports that only describe something and never carry a failure of their
// own. A new read-only report goes on this list.
mod read_only {
    use super::Outcome;
    use crate::{cli::DocsReport, ops};
    use pocketeer_core::{card, library};

    impl Outcome for card::Status {}
    impl Outcome for card::bios::BiosReport {}
    impl Outcome for card::cores::CoreListReport {}
    impl Outcome for card::cores::CoreInfo {}
    impl Outcome for library::Status {}
    impl Outcome for library::covers::FetchReport {}
    impl Outcome for ops::card::RefreshReport {}
    impl Outcome for ops::cores::SearchReport {}
    impl Outcome for ops::library::InitReport {}
    impl Outcome for ops::library::ListReport {}
    impl Outcome for ops::library::CoversStatusReport {}
    impl Outcome for ops::saves::ListReport {}
    impl Outcome for ops::saves::VersionsReport {}
    impl Outcome for ops::memories::MemoriesReport {}
    impl Outcome for ops::context::ConfigPathReport {}
    impl Outcome for ops::context::ConfigShowReport {}
    impl Outcome for DocsReport {}
}

pub struct Output {
    json: bool,
}

impl Output {
    pub fn new(json: bool) -> Self {
        Output { json }
    }

    /// A command line that can't be acted on (clap error, no subcommand).
    /// JSON mode still gets its one envelope, with a `USAGE` error; the
    /// caller has already sent clap's own text (or the help) to stderr.
    /// Exit 2, like clap.
    pub fn usage(&self, message: impl Into<String>, hint: Option<String>) -> ExitCode {
        let error = Error::Usage {
            message: message.into(),
            hint,
        }
        .to_info();
        if self.json {
            print_json(&Envelope::<()> {
                schema_version: SCHEMA_VERSION,
                ok: false,
                data: None,
                error: Some(error),
            });
        }
        ExitCode::from(2)
    }

    /// A command that failed before it had anything to report.
    pub fn fail(&self, error: Error) -> ExitCode {
        if self.json {
            print_json(&Envelope::<()> {
                schema_version: SCHEMA_VERSION,
                ok: false,
                data: None,
                error: Some(error.to_info()),
            });
        } else {
            print_error(&error.to_info());
        }
        ExitCode::FAILURE
    }

    /// A command's result: its data (JSON, or text for people) and its
    /// failure, if the data reports one or the command failed outright.
    pub fn emit<T: Outcome + Text>(&self, result: Result<T, Error>) -> ExitCode {
        let (data, error) = match &result {
            Ok(data) => (Some(data), data.error().map(|e| e.to_info())),
            Err(err) => (None, Some(err.to_info())),
        };

        if self.json {
            print_json(&Envelope {
                schema_version: SCHEMA_VERSION,
                ok: error.is_none(),
                data,
                error: error.clone(),
            });
        } else {
            if let Some(data) = data {
                print!("{}", data.text(Reader::Cli));
                let _ = std::io::stdout().flush();
            }
            if let Some(err) = &error {
                print_error(err);
            }
        }

        if error.is_none() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }

    /// Progress reporter for stderr; silent in JSON mode or when stderr is piped.
    pub fn progress(&self) -> Box<dyn FnMut(Event)> {
        if self.json || !std::io::stderr().is_terminal() {
            return Box::new(|_| {});
        }
        let mut last_pct: Option<u64> = None;
        let mut line_open = false;
        Box::new(move |event| match event {
            Event::Download {
                received, total, ..
            } => {
                let pct = total.filter(|t| *t > 0).map(|t| received * 100 / t);
                if pct != last_pct {
                    last_pct = pct;
                    match pct {
                        Some(p) => eprint!("\r  downloading {p:3}%"),
                        None => eprint!("\r  downloading {} KiB", received / 1024),
                    }
                    line_open = true;
                }
            }
            Event::Status(s) => {
                if line_open {
                    eprintln!();
                    line_open = false;
                }
                last_pct = None;
                eprintln!("  {s}");
            }
            Event::Step { .. } => {}
        })
    }
}

fn print_error(err: &ErrorInfo) {
    eprintln!("error: {}", err.message);
    if let Some(hint) = &err.hint {
        eprintln!("hint: {hint}");
    }
}

fn print_json<T: Serialize>(value: &T) {
    // Serializing our own plain structs can't fail.
    let text = serde_json::to_string_pretty(value).expect("serializable output");
    println!("{text}");
}
