//! Settings (`pocketeer config`) and the reference (`pocketeer docs`).

use std::{fmt::Write, path::PathBuf};

use clap::{CommandFactory, Subcommand};
use pocketeer_core::{
    Error, Result,
    config::{Config, ConfigKey},
    error::CODES,
};
use serde::Serialize;

use super::{
    Cli,
    output::{Outcome, SCHEMA_VERSION},
};
use crate::{
    ops::context::Context,
    text::{Text, table},
};

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show where the config file is.
    Path,
    /// Show all settings, or one.
    Get {
        /// The setting (default: all).
        #[arg(value_name = "KEY", value_parser = config_key())]
        key: Option<ConfigKey>,
    },
    /// Set a value (the GitHub token is stored in plain text).
    Set {
        /// The setting to change.
        #[arg(value_name = "KEY", value_parser = config_key())]
        key: ConfigKey,
        /// New value (a path for library, cache-dir and root).
        #[arg(value_name = "VALUE")]
        value: String,
    },
    /// Remove a value.
    Unset {
        /// The setting to clear.
        #[arg(value_name = "KEY", value_parser = config_key())]
        key: ConfigKey,
    },
}

/// Config keys by name; `--help` lists them.
fn config_key() -> impl clap::builder::TypedValueParser<Value = ConfigKey> {
    use clap::builder::TypedValueParser;
    clap::builder::PossibleValuesParser::new(ConfigKey::ALL.map(ConfigKey::name))
        .map(|name| ConfigKey::from_name(&name).expect("one of the listed names"))
}

pub fn run(ctx: &Context, cmd: ConfigCommand) -> Result<Outcome> {
    match cmd {
        ConfigCommand::Path => Ok(Outcome::report(&config_path(ctx)?)),
        ConfigCommand::Get { key } => Ok(Outcome::report(&config_get(ctx, key)?)),
        ConfigCommand::Set { key, value } => {
            Ok(Outcome::report(&config_set(ctx, key, Some(&value))?))
        }
        ConfigCommand::Unset { key } => Ok(Outcome::report(&config_set(ctx, key, None)?)),
    }
}

#[derive(Debug, Serialize)]
pub struct ConfigPathReport {
    pub path: PathBuf,
    pub exists: bool,
}

pub fn config_path(ctx: &Context) -> Result<ConfigPathReport> {
    let path = ctx.config_path()?;
    Ok(ConfigPathReport {
        exists: path.is_file(),
        path,
    })
}

#[derive(Debug, Serialize)]
pub struct ConfigShowReport {
    pub path: PathBuf,
    /// Only the requested key when one was given.
    pub values: Vec<ConfigEntry>,
}

#[derive(Debug, Serialize)]
pub struct ConfigEntry {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

fn show(config: &Config, path: PathBuf, only: Option<ConfigKey>) -> ConfigShowReport {
    let keys = only.map_or(ConfigKey::ALL.to_vec(), |k| vec![k]);
    let values = keys
        .into_iter()
        .map(|key| {
            let mut value = config.get(key);
            // A token is shown only far enough to recognise it.
            if key == ConfigKey::GithubToken
                && let Some(v) = &value
            {
                value = Some(format!("{}…", &v[..v.len().min(4)]));
            }
            ConfigEntry {
                key: key.name().to_string(),
                value,
            }
        })
        .collect();
    ConfigShowReport { path, values }
}

pub fn config_get(ctx: &Context, key: Option<ConfigKey>) -> Result<ConfigShowReport> {
    Ok(show(ctx.config()?, ctx.config_path()?, key))
}

/// `value: None` unsets the key.
pub fn config_set(ctx: &Context, key: ConfigKey, value: Option<&str>) -> Result<ConfigShowReport> {
    let path = ctx.config_path()?;
    let mut config = ctx.config()?.clone();
    config.set(key, value);
    config.save(&path)?;
    Ok(show(&config, path, Some(key)))
}

impl Text for ConfigPathReport {
    fn text(&self) -> String {
        format!(
            "{}{}\n",
            self.path.display(),
            if self.exists {
                ""
            } else {
                "  (not created yet)"
            }
        )
    }
}

impl Text for ConfigShowReport {
    fn text(&self) -> String {
        let mut out = String::new();
        let rows: Vec<[String; 2]> = self
            .values
            .iter()
            .map(|e| {
                [
                    e.key.clone(),
                    e.value.clone().unwrap_or_else(|| "(unset)".into()),
                ]
            })
            .collect();
        table(&mut out, ["KEY", "VALUE"], &rows);
        let _ = writeln!(out, "\nConfig file: {}", self.path.display());
        out
    }
}

impl Text for DocsReport {
    fn text(&self) -> String {
        self.markdown.clone()
    }
}

// ---------------------------------------------------------------------------
// `pocketeer docs`: the whole CLI reference, generated from the clap command
// tree so it can never drift from the real flags.

#[derive(Debug, Serialize)]
pub struct DocsReport {
    pub markdown: String,
    pub commands: Vec<String>,
    pub error_codes: Vec<ErrorCode>,
}

#[derive(Debug, Serialize)]
pub struct ErrorCode {
    pub code: &'static str,
    pub meaning: &'static str,
}

/// The hand-written part of the reference; the rest is generated from clap.
const CONVENTIONS: &str = include_str!("../../docs/conventions.md");

pub fn docs(path: &[String]) -> Result<Outcome> {
    let mut root = Cli::command();
    root.build();

    // Narrow to a subtree if asked.
    let mut target: &mut clap::Command = &mut root;
    let mut prefix = vec!["pocketeer".to_string()];
    for name in path {
        target = target
            .find_subcommand_mut(name)
            .ok_or_else(|| Error::Usage {
                message: format!("no command `{}`", path.join(" ")),
                hint: Some("run `pocketeer docs` for the full list".into()),
            })?;
        prefix.push(name.clone());
    }

    let mut md = String::new();
    let mut commands = Vec::new();
    if path.is_empty() {
        md.push_str(&format!(
            "# pocketeer CLI reference\n\nSchema version {SCHEMA_VERSION}. Generated by `pocketeer docs`.\n\n"
        ));
        md.push_str(CONVENTIONS);
        md.push_str("\n## Commands\n\n");
    } else {
        md.push_str(&format!("# {}\n\n", prefix.join(" ")));
    }
    walk(target, &prefix, &mut md, &mut commands);

    if path.is_empty() {
        md.push_str("## Error codes\n\n| code | meaning |\n|---|---|\n");
        for (code, meaning) in CODES {
            md.push_str(&format!("| `{code}` | {meaning} |\n"));
        }
    }

    Ok(Outcome::report(&DocsReport {
        markdown: md,
        commands,
        error_codes: CODES
            .iter()
            .map(|(code, meaning)| ErrorCode { code, meaning })
            .collect(),
    }))
}

fn walk(cmd: &mut clap::Command, prefix: &[String], md: &mut String, commands: &mut Vec<String>) {
    let full = prefix.join(" ");
    let has_children = cmd.get_subcommands().any(|c| c.get_name() != "help");
    md.push_str(&format!("### `{full}`\n\n```text\n"));
    md.push_str(cmd.render_long_help().to_string().trim_end());
    md.push_str("\n```\n\n");
    if !has_children {
        commands.push(full.clone());
    }
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .filter(|n| n != "help")
        .collect();
    for name in names {
        let mut next = prefix.to_vec();
        next.push(name.clone());
        let sub = cmd.find_subcommand_mut(&name).expect("listed subcommand");
        walk(sub, &next, md, commands);
    }
}
