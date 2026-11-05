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
mod lifecycle;
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
mod status;
#[cfg(test)]
mod testing;
mod vm_record;

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand};
use config::{Paths, env_var};
use error::Result;
use hangar::Hangar;
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
    /// Create or start bays and proxies and apply the config
    #[command(after_help = "Without NAME: every bay and proxy. A bay's \
                            proxy comes up first; a bay whose proxy fails \
                            is left alone. Exits 1 if anything failed.")]
    Up {
        /// Bays or proxies (default: all)
        names: Vec<String>,
    },
    /// Stop bays and proxies
    Down {
        /// Bays or proxies (default: all)
        names: Vec<String>,
    },
    /// Stop, then start bays and proxies
    Restart {
        /// Bays or proxies (default: all)
        names: Vec<String>,
    },
    /// Remove a bay's or proxy's VM; --data also deletes its data
    #[command(after_help = "--data deletes a bay's home and data volume, \
                            or a proxy's data dir. Package caches are kept.")]
    Rm {
        /// A bay or proxy, configured or left over
        name: String,
        /// Also delete its data
        #[arg(long)]
        data: bool,
        /// Don't ask before deleting data
        #[arg(short, long)]
        yes: bool,
    },
    /// List bays and proxies
    Ps,
    /// Show everything about a bay or proxy
    Inspect {
        /// A bay or proxy
        name: String,
    },
    /// Open a login shell in a bay, or run CMD there
    Exec {
        /// The bay
        bay: String,
        /// Command and its arguments, passed as given
        #[arg(
            value_name = "CMD",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        args: Vec<String>,
    },
    /// Show hangar's log, a bay's apps' journal or a proxy's log
    Logs {
        /// A bay or proxy (default: hangar's own log)
        name: Option<String>,
        /// One app of the bay
        app: Option<String>,
        /// Keep following
        #[arg(short, long)]
        follow: bool,
    },
    /// Copy files into or out of a running bay
    #[command(
        after_help = "Examples:\n  hangar cp ~/notes.md work:~/notes.md\n  \
                            hangar cp work:~/out.txt ~/out.txt"
    )]
    Cp {
        /// Host path or BAY:PATH
        src: String,
        /// Host path or BAY:PATH
        dest: String,
    },
    /// Run an app's one-time interactive setup in a bay
    Setup {
        /// The bay
        bay: String,
        /// An enabled app with a setup
        app: String,
        /// Run it even if the check already passes
        #[arg(long)]
        force: bool,
    },
}

impl Command {
    fn changes_state(&self) -> bool {
        matches!(
            self,
            Self::Up { .. }
                | Self::Down { .. }
                | Self::Restart { .. }
                | Self::Rm { .. }
                | Self::Cp { .. }
                | Self::Setup { .. }
        )
    }
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
    match run(cli.command) {
        Ok(outcome) => {
            output::emit(format, &outcome);
            ExitCode::from(outcome.exit_code())
        }
        Err(error) => {
            output::emit_error(format, &error);
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<Outcome> {
    let paths = Paths::locate(&env_var)?;
    if let Command::Init { force } = command {
        commands::init(&paths, force)?;
        return Ok(Outcome::Done);
    }
    let hangar = Hangar::load(&paths)?;
    let _lock = if command.changes_state() {
        let path = hangar.state_root.join("lock");
        Some(lock::acquire(&path, || {
            log::info!("waiting for another hangar");
        })?)
    } else {
        None
    };
    let hangar = &hangar;
    Ok(match command {
        Command::Init { .. } => Outcome::Done,
        Command::Up { names } => {
            Outcome::Changes(commands::up(hangar, &names)?)
        }
        Command::Down { names } => {
            Outcome::Changes(commands::down(hangar, &names)?)
        }
        Command::Restart { names } => {
            Outcome::Changes(commands::restart(hangar, &names)?)
        }
        Command::Rm { name, data, yes } => {
            Outcome::Changes(commands::rm(hangar, &name, data, yes)?)
        }
        Command::Ps => Outcome::Overview(commands::ps(hangar)),
        Command::Inspect { name } => {
            Outcome::Detail(commands::inspect(hangar, &name)?)
        }
        Command::Exec { bay, args } => {
            commands::exec(hangar, &bay, &args)?;
            Outcome::Streamed
        }
        Command::Logs { name, app, follow } => {
            commands::logs(hangar, name.as_deref(), app.as_deref(), follow)?;
            Outcome::Streamed
        }
        Command::Cp { src, dest } => {
            Outcome::Copied(commands::cp(hangar, &src, &dest)?)
        }
        Command::Setup { bay, app, force } => {
            commands::setup(hangar, &bay, &app, force)?;
            Outcome::Streamed
        }
    })
}
