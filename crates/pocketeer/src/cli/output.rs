//! How a command's outcome reaches the user: one JSON envelope on stdout
//! with `--json`; otherwise the text on stdout, then what to do next, and
//! errors and progress on stderr. Also how a command that changes something
//! runs: plan, apply unless `--dry-run`, report both.

use std::{
    io::{IsTerminal, Write},
    process::ExitCode,
};

use pocketeer_core::{
    Error, ErrorInfo, Remedy, Result,
    plan::{Applied, Event},
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    ops::{Batch, BatchState},
    text::Text,
};

/// Bump when a released JSON shape changes incompatibly.
pub const SCHEMA_VERSION: u32 = 2;

#[derive(Serialize)]
struct Envelope<'a> {
    schema_version: u32,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorInfo>,
}

/// What a command produced: its data, how it reads, what to do next, and
/// the failure it ends with (a report can carry both data and a failure:
/// a plan with conflicts left, a batch where some items failed).
pub struct Outcome {
    data: Value,
    text: String,
    hints: Vec<String>,
    error: Option<Error>,
}

impl Outcome {
    /// A report that only describes something.
    pub fn report<T: Serialize + Text>(report: &T) -> Self {
        Outcome {
            data: serde_json::to_value(report).expect("our reports serialise"),
            text: report.text(),
            hints: Vec::new(),
            error: None,
        }
    }

    /// Ends the command with `error` (exit 1), the data still shown.
    pub fn failing(mut self, error: Option<Error>) -> Self {
        if error.is_some() {
            self.error = error;
        }
        self
    }

    /// A next step for someone at the command line, printed after the text
    /// (never in JSON) when `when` holds.
    pub fn hint(mut self, when: bool, hint: impl Into<String>) -> Self {
        if when {
            self.hints.push(hint.into());
        }
        self
    }

    /// The remedy of an error the data reports, as a hint.
    pub fn remedy(self, remedy: Option<Remedy>) -> Self {
        match remedy {
            Some(r) => self.hint(true, remedy_hint(r)),
            None => self,
        }
    }

    /// More text after the report's own.
    pub fn then_text(mut self, more: &str) -> Self {
        self.text.push_str(more);
        self
    }

    /// One more field in the JSON data.
    pub fn with(mut self, key: &str, value: impl Serialize) -> Self {
        if let Value::Object(map) = &mut self.data {
            let value = serde_json::to_value(value).expect("our reports serialise");
            if !value.is_null() && value != Value::Array(Vec::new()) {
                map.insert(key.into(), value);
            }
        }
        self
    }
}

/// `--dry-run` or not.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Plans only.
    DryRun,
    /// The default: plans applied.
    Applied,
}

/// The one rule behind every command's exit status: any failure is an
/// error, otherwise success (a dry run included).
pub fn verdict(changes: usize, failed: usize) -> Option<Error> {
    (failed > 0).then_some(Error::SomeFailed {
        failed,
        total: failed + changes,
    })
}

/// A plan and, once applied, its result. Reads as the plan, then what
/// applying did (or that it was a dry run); serialises as
/// `{mode, ...plan fields, result?}`.
#[derive(Serialize)]
pub struct Staged<P, R> {
    pub mode: Mode,
    #[serde(flatten)]
    pub plan: P,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<R>,
    #[serde(skip)]
    changes: usize,
    #[serde(skip)]
    errors: usize,
}

/// Applies `plan` unless this is a dry run or there is nothing to do.
pub fn stage<P, R>(
    mode: Mode,
    changes: usize,
    mut plan: P,
    apply: impl FnOnce(&mut P) -> Result<R>,
) -> Result<Staged<P, R>> {
    let result = if mode == Mode::Applied && changes > 0 {
        Some(apply(&mut plan)?)
    } else {
        None
    };
    Ok(Staged {
        mode,
        plan,
        result,
        changes,
        errors: 0,
    })
}

impl<P: Serialize + Text, R: Applied + Text> Staged<P, R> {
    /// Unresolved planning problems also fail a dry run or a zero-change run.
    pub fn planning_errors(mut self, errors: usize) -> Self {
        self.errors = errors;
        self
    }

    pub fn outcome(&self) -> Outcome {
        let failed = self.result.as_ref().map_or(0, Applied::failed) + self.errors;
        let mut text = self.plan.text();
        match &self.result {
            Some(result) => text.push_str(&result.text()),
            None => text.push_str(dry_run_line(self.mode, self.changes)),
        }
        Outcome {
            data: serde_json::to_value(self).expect("our reports serialise"),
            text,
            hints: Vec::new(),
            error: verdict(self.changes, failed),
        }
    }
}

/// Applies every item of `batch` unless this is a dry run, and reports it:
/// `{mode, items}`, each item's text, and its remedy when it failed.
pub fn stage_batch<P: Serialize + Text, R: Serialize + Text>(
    mode: Mode,
    batch: Batch<P, R>,
    count: impl Fn(&P) -> usize,
    apply: impl FnMut(&mut P) -> Result<R>,
) -> Outcome {
    let batch = match mode {
        Mode::Applied => batch.apply(&count, apply),
        Mode::DryRun => batch,
    };
    #[derive(Serialize)]
    struct Shape<'a, P, R> {
        mode: Mode,
        #[serde(flatten)]
        batch: &'a Batch<P, R>,
    }
    let changes = batch.changes(count);
    let applied = batch
        .items
        .iter()
        .any(|i| matches!(i.state, BatchState::Applied { .. }));
    let mut text = batch.text();
    if !applied {
        text.push_str(dry_run_line(mode, changes));
    }
    let remedies = batch.items.iter().filter_map(|i| match &i.state {
        BatchState::Unplanned { error } | BatchState::Failed { error, .. } => error.remedy,
        _ => None,
    });
    let mut hints: Vec<String> = Vec::new();
    for r in remedies {
        let hint = remedy_hint(r).to_string();
        if !hints.contains(&hint) {
            hints.push(hint);
        }
    }
    Outcome {
        data: serde_json::to_value(Shape {
            mode,
            batch: &batch,
        })
        .expect("our reports serialise"),
        text,
        hints,
        error: verdict(changes, batch.failed()),
    }
}

/// A dry run that would have changed something says so.
fn dry_run_line(mode: Mode, changes: usize) -> &'static str {
    if mode == Mode::DryRun && changes > 0 {
        "\nDry run: nothing was changed.\n"
    } else {
        ""
    }
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
        };
        if self.json {
            print_json(&Envelope {
                schema_version: SCHEMA_VERSION,
                ok: false,
                data: None,
                error: Some(described(&error)),
            });
        }
        ExitCode::from(2)
    }

    /// A command's outcome: its data (JSON, or text and hints for people)
    /// and its failure, if it reports one or the command failed outright.
    pub fn emit(&self, result: Result<Outcome>) -> ExitCode {
        let (outcome, error) = match &result {
            Ok(o) => (Some(o), o.error.as_ref().map(described)),
            Err(e) => (None, Some(described(e))),
        };
        if self.json {
            print_json(&Envelope {
                schema_version: SCHEMA_VERSION,
                ok: error.is_none(),
                data: outcome.map(|o| &o.data),
                error: error.clone(),
            });
        } else {
            if let Some(o) = outcome {
                print!("{}", o.text);
                // What to do next stands apart from what was found.
                if !o.hints.is_empty() && !o.text.is_empty() {
                    println!();
                }
                for hint in &o.hints {
                    println!("{hint}");
                }
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

/// The error as the command line reports it: its remedy in words.
fn described(error: &Error) -> ErrorInfo {
    let mut info = error.to_info();
    if info.hint.is_none() {
        info.hint = info.remedy.map(|r| remedy_hint(r).to_string());
    }
    info
}

/// What to do next, said for someone at the command line.
pub fn remedy_hint(remedy: Remedy) -> &'static str {
    match remedy {
        Remedy::InsertCard => "Insert the SD card (or mount the Pocket), or pass --root <PATH>",
        Remedy::ChooseCard => "Pass --root <PATH> to choose one",
        Remedy::CheckCardRoot => {
            "A Pocket SD card has Analogue_Pocket.json, Assets/ and Cores/ at its root"
        }
        Remedy::GoOnline => "Run once without --offline to populate the cache",
        Remedy::SetGithubToken => {
            "GitHub rate limit? Set POCKETEER_GITHUB_TOKEN or pass --github-token"
        }
        Remedy::SearchCores => "Use `pocketeer cores search <query>` to find the exact id",
        Remedy::ListInstalledCores => "Use `pocketeer cores list` to see installed cores",
        Remedy::PickVersion => "Pick one of the listed versions, or omit @version for the latest",
        Remedy::InspectStaging => {
            "Some changes could not be rolled back; inspect the reported paths. Core install recovery files, if any, are under .pocketeer/staging"
        }
        Remedy::CreateLibrary => {
            "Run `pocketeer library init`, or point at one with --library / `pocketeer config set library <DIR>`"
        }
        Remedy::UpgradePocketeer => "Upgrade pocketeer; downgrading the library is not supported",
        Remedy::FindRom => "Use `pocketeer library list [query]` to find the file name or id",
        Remedy::PickRom => "Pass the ROM id (from `pocketeer library list --json`) or --platform",
        Remedy::Replan => "Run the command again to plan from the current state",
        Remedy::Retry => "Retry once nothing else is writing to it",
        Remedy::ChooseImage => {
            "Choose a PNG, JPEG or WebP image (up to 32 MiB and 8192 × 8192 pixels)"
        }
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
    let text = serde_json::to_string_pretty(value).expect("serializable");
    println!("{text}");
}
