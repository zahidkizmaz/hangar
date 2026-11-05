//! hangar: locked-down bays for AI coding agents, in a sandbox.

#![expect(dead_code, reason = "rewrite in progress; removed in Task 16")]

mod apps;
mod bay;
mod commands;
mod config;
mod dirs;
mod error;
mod files;
mod hangar;
mod http;
mod json;
mod lock;
mod logger;
mod mounts;
mod output;
mod packages;
mod process;
mod proxy;
mod sandbox;
mod secret;
mod state;
#[cfg(test)]
mod testing;
mod vm_record;

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand};
use config::{Paths, env_var};
use error::Result;
use output::{Format, Outcome};

/// Sandboxed workspaces for AI agents.
#[derive(Debug, Parser)]
#[command(
    version,
    propagate_version = true,
    after_help = "Exit codes: 0 ok, 1 error, 2 usage.\n\
                  Scripts and agents: add --json, see docs/cli.md."
)]
#[expect(clippy::doc_markdown, reason = "doc comments are the --help text")]
struct Cli {
    /// Only print errors
    #[arg(short, long, global = true)]
    quiet: bool,
    /// More detail: -v for debug, -vv for trace (or set HANGAR_LOG)
    #[arg(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,
    /// Machine-readable output on stdout, errors as JSON on stderr
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
#[expect(clippy::doc_markdown, reason = "doc comments are the --help text")]
enum Command {
    /// Write a starter $XDG_CONFIG_HOME/hangar/hangar.json
    Init {
        /// Overwrite an existing file
        #[arg(long)]
        force: bool,
    },
}

fn main() -> ExitCode {
    // Usage errors exit 2, help and version 0 (clap's defaults).
    let cli = Cli::parse();
    let env = env_var("HANGAR_LOG");
    let level = logger::level(cli.quiet, cli.verbose.into(), env.as_deref());
    // Color only on a terminal, and never with NO_COLOR set.
    let color =
        io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    logger::init(level, color);
    let format = if cli.json {
        Format::Json
    } else {
        Format::Human
    };
    match run(&cli.command) {
        Ok(outcome) => {
            output::emit(format, &outcome);
            ExitCode::SUCCESS
        }
        Err(error) => {
            output::emit_error(format, &error);
            ExitCode::FAILURE
        }
    }
}

fn run(command: &Command) -> Result<Outcome> {
    let paths = Paths::locate(&env_var)?;
    match command {
        Command::Init { force } => commands::init(&paths, *force)?,
    }
    Ok(Outcome::Done)
}
