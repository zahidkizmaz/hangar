//! hangar: locked-down bays for AI coding agents, in a sandbox.

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
mod keychain;
mod lifecycle;
mod lock;
mod logger;
mod mounts;
mod output;
mod packages;
mod process;
mod prompt;
mod proxy;
mod sandbox;
mod secret;
mod state;
mod status;
#[cfg(test)]
mod testing;
mod vm_record;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand};
use config::{Paths, env_var};
use dirs::Base;
use error::Result;
use hangar::Hangar;
use output::{Format, Outcome};
use prompt::Terminal;

/// Sandboxed workspaces for AI agents.
#[derive(Debug, Parser)]
#[command(
    version,
    propagate_version = true,
    after_help = "Exit codes: 0 ok, 1 error, 2 usage, 3 ps found something \
                  wrong.\n\
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
    #[command(flatten)]
    OnHangar(OnHangar),
}

/// The commands that act on the configured hangar.
#[derive(Debug, Subcommand)]
enum OnHangar {
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
    /// Manage a proxy's tokens in the OS keychain
    #[command(subcommand)]
    Secret(SecretCommand),
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

#[derive(Debug, Subcommand)]
enum SecretCommand {
    /// Ask for a proxy's tokens and store them in the keychain
    #[command(after_help = "Typed unseen on a terminal; otherwise one \
                            token per line on stdin, in the order the \
                            proxy kind lists them (docs/proxy-kinds.md).")]
    Set {
        /// The proxy
        proxy: String,
        /// Only this token, e.g. session-token (default: all)
        token: Option<String>,
    },
}

impl OnHangar {
    fn changes_state(&self) -> bool {
        matches!(
            self,
            Self::Up { .. }
                | Self::Down { .. }
                | Self::Restart { .. }
                | Self::Rm { .. }
                | Self::Cp { .. }
                | Self::Secret(_)
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
    logger::init(level, color, log_file());
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

fn log_file() -> Option<PathBuf> {
    let dir = dirs::hangar_dir(Base::State, &env_var).ok()?;
    Some(dir.join(commands::LOG_FILE))
}

fn run(command: Command) -> Result<Outcome> {
    let paths = Paths::locate(&env_var)?;
    match command {
        Command::Init { force } => {
            commands::init(&paths, force)?;
            Ok(Outcome::Done)
        }
        Command::OnHangar(command) => on_hangar(&paths, command),
    }
}

fn on_hangar(paths: &Paths, command: OnHangar) -> Result<Outcome> {
    let hangar = Hangar::load(paths)?;
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
        OnHangar::Up { names } => {
            Outcome::Changes(commands::up(hangar, &names, &Terminal)?)
        }
        OnHangar::Down { names } => {
            Outcome::Changes(commands::down(hangar, &names)?)
        }
        OnHangar::Restart { names } => {
            Outcome::Changes(commands::restart(hangar, &names, &Terminal)?)
        }
        OnHangar::Secret(SecretCommand::Set { proxy, token }) => {
            Outcome::Changes(commands::secret_set(
                hangar,
                &proxy,
                token.as_deref(),
                &Terminal,
            )?)
        }
        OnHangar::Rm { name, data, yes } => {
            Outcome::Changes(commands::rm(hangar, &name, data, yes, &Terminal)?)
        }
        OnHangar::Ps => Outcome::Overview(commands::ps(hangar)),
        OnHangar::Inspect { name } => {
            Outcome::Detail(commands::inspect(hangar, &name)?)
        }
        OnHangar::Exec { bay, args } => {
            commands::exec(hangar, &bay, &args)?;
            Outcome::Streamed
        }
        OnHangar::Logs { name, app, follow } => {
            commands::logs(hangar, name.as_deref(), app.as_deref(), follow)?;
            Outcome::Streamed
        }
        OnHangar::Cp { src, dest } => {
            Outcome::Copied(commands::cp(hangar, &src, &dest)?)
        }
        OnHangar::Setup { bay, app, force } => {
            commands::setup(hangar, &bay, &app, force)?;
            Outcome::Streamed
        }
    })
}
