//! pocketeer: the command line, or the TUI when no command is given.

mod cli;
mod ops;
mod text;
mod tui;

fn main() -> std::process::ExitCode {
    cli::main()
}
