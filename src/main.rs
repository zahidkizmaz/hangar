//! hangar: locked-down bays for AI coding agents, and a tower that holds
//! their credentials, in a sandbox.

mod apps;
mod bay;
mod broker;
mod commands;
mod config;
mod credential;
mod dirs;
mod error;
mod files;
mod hangar;
mod http;
mod json;
mod keychain;
mod lock;
mod logger;
mod mounts;
mod output;
mod overview;
mod packages;
mod password;
mod process;
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
use hangar::Hangar;
use output::{Format, Outcome};
use overview::StatusReport;

/// Sandboxed workspaces for AI agents.
#[derive(Debug, Parser)]
#[command(
    version,
    propagate_version = true,
    after_help = "Exit codes: 0 ok, 1 error, 2 usage, 3 status: not healthy.\n\
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
    #[command(after_help = "init writes hangar's own config; 'hangar setup \
                            NAME' configures an app inside a bay.")]
    Init {
        /// Overwrite an existing file
        #[arg(long)]
        force: bool,
    },
    /// Start the tower and the bays and apply the config
    #[command(after_help = "Without BAY: every configured bay. A failing \
                            bay doesn't stop the others; up exits 1 and \
                            --json lists it under failed.\n\n\
                            Examples:\n  hangar up\n  hangar up work\n  \
                            hangar up --json | jq .failed")]
    Up {
        /// Bays to bring up (default: all)
        bays: Vec<String>,
    },
    /// Stop bays, or every bay and the tower
    #[command(after_help = "Without BAY: every bay, then the tower. With \
                            BAY: only those bays; the tower keeps running.")]
    Down {
        /// Bays to stop (default: all, and the tower)
        bays: Vec<String>,
    },
    /// Show the tower, each bay (apps, run entries, mounts) and ports
    #[command(after_help = "Exits 3 when something isn't healthy.\n\n\
                            Examples:\n  hangar status\n  \
                            hangar status --json | jq .tower\n  \
                            hangar status --json || hangar up")]
    Status,
    /// Open a login shell in the bay, or run CMD there
    #[command(after_help = "Examples:\n  hangar shell\n  \
                            hangar shell gh --help\n  \
                            hangar shell git -C repo status")]
    Shell {
        #[command(flatten)]
        bay: BayFlag,
        /// Command and its arguments, passed as given (including --help)
        #[arg(
            value_name = "CMD",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        args: Vec<String>,
    },
    /// Show the log of a run entry or app
    #[command(after_help = "Examples:\n  hangar logs web\n  \
                            hangar logs -f web")]
    Logs {
        #[command(flatten)]
        bay: BayFlag,
        /// Keep following the log
        #[arg(short, long)]
        follow: bool,
        /// The run entry or app
        name: String,
    },
    /// Run an app's one-time interactive setup in the bay
    #[command(after_help = "Runs the app's setup command with a terminal, \
                            then its check; a failing check is an error. \
                            hangar up skips an app's run entry while its \
                            check fails. To write hangar's own config, use \
                            'hangar init'.\n\n\
                            Examples:\n  hangar setup web\n  \
                            hangar setup --force web")]
    Setup {
        #[command(flatten)]
        bay: BayFlag,
        /// Run the setup command even if the check already passes
        #[arg(long)]
        force: bool,
        /// An enabled app with a setup
        name: String,
    },
    /// Copy the declared files into the bay, or one SRC
    #[command(after_help = "Without SRC: the declared files only (write \
                            what changed, remove dropped ones).\n\
                            With SRC: one host file or directory, same \
                            checks, not managed; a declared entry for the \
                            same path overwrites it on the next copy, up or \
                            restart.\n\n\
                            Examples:\n  hangar copy\n  \
                            hangar copy ~/notes/todo.md\n  \
                            hangar copy ./script.sh ~/bin/script.sh")]
    Copy {
        #[command(flatten)]
        bay: BayFlag,
        /// Host file or directory (default: the declared files)
        src: Option<String>,
        /// VM path (default: SRC's place under the VM user's home)
        #[arg(requires = "src")]
        dest: Option<String>,
    },
    /// Re-apply files and the env file, then restart run entries
    #[command(after_help = "Without NAME: every run entry. hangar up \
                            never restarts anything; it warns when a running \
                            entry's inputs changed.\n\n\
                            Examples:\n  hangar restart\n  \
                            hangar restart web\n  \
                            hangar restart --no-copy web")]
    Restart {
        #[command(flatten)]
        bay: BayFlag,
        /// Don't re-copy files (the env file is still refreshed)
        #[arg(long)]
        no_copy: bool,
        /// run entries or apps (default: all)
        names: Vec<String>,
    },
    /// Copy the vault login to the clipboard and open the vault UI
    #[command(after_help = "Examples:\n  hangar vault-ui\n  \
                            hangar vault-ui --json | jq -r .url")]
    VaultUi,
    /// Manage your own credentials in the vault
    #[command(subcommand)]
    Credential(CredentialCommand),
    /// Remove bays, or every bay and the tower
    #[command(after_help = "Without BAY: every VM; --state also deletes \
                            stateDir and the keychain item. With BAY: only \
                            those bays (configured or not); --state also \
                            deletes their folders. Package caches are kept.\n\n\
                            Examples:\n  hangar destroy && hangar up\n  \
                            hangar destroy old --state\n  \
                            hangar destroy --state --yes")]
    Destroy {
        /// Bays to remove (default: every VM)
        bays: Vec<String>,
        /// Also delete hangar's data: without BAY the vault, tokens, every
        /// bay's home and the master password; with BAY those bays' folders
        #[arg(long)]
        state: bool,
        /// Don't ask before deleting state
        #[arg(long)]
        yes: bool,
    },
}

/// Which bay a single-bay command acts on.
#[derive(Debug, clap::Args)]
struct BayFlag {
    /// The bay (default: the only one)
    #[arg(short = 'b', long = "bay", value_name = "NAME")]
    bay: Option<String>,
}

#[derive(Debug, Subcommand)]
#[expect(clippy::doc_markdown, reason = "doc comments are the --help text")]
enum CredentialCommand {
    /// Store a credential (hidden prompt, or stdin)
    #[command(after_help = "Examples:\n  \
                            hangar credential set CLAUDE_CODE_OAUTH_TOKEN\n  \
                            hangar credential set GITHUB_TOKEN < token.txt")]
    Set {
        /// UPPER_SNAKE_CASE, named after the variable the tool reads
        name: String,
    },
    /// List credential names (never values)
    #[command(after_help = "Examples:\n  hangar credential list\n  \
                            hangar credential list --json | jq -r '.[].name'")]
    List,
    /// Delete a credential you stored
    #[command(after_help = "Example:\n  hangar credential rm GITHUB_TOKEN")]
    Rm {
        /// The credential to delete
        name: String,
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
        Some(hold_lock()?)
    } else {
        None
    };
    Ok(match command {
        Command::Init { .. } => unreachable!("handled above"),
        Command::Up { bays } => Outcome::Up(commands::up(&hangar, &bays)?),
        Command::Down { bays } => done(commands::down(&hangar, &bays))?,
        Command::Status => Outcome::Status(StatusReport::collect(&hangar)),
        Command::Shell { bay, args } => {
            streamed(commands::shell(&hangar, bay.bay.as_deref(), &args))?
        }
        Command::Logs { bay, follow, name } => streamed(commands::logs(
            &hangar,
            bay.bay.as_deref(),
            &name,
            follow,
        ))?,
        Command::Setup { bay, force, name } => streamed(commands::setup(
            &hangar,
            bay.bay.as_deref(),
            &name,
            force,
        ))?,
        Command::Copy { bay, src, dest } => {
            let (bay, copied) = commands::copy_files(
                &hangar,
                bay.bay.as_deref(),
                src.as_deref(),
                dest.as_deref(),
            )?;
            Outcome::Copied(bay, copied)
        }
        Command::Restart {
            bay,
            no_copy,
            names,
        } => {
            let (bay, restarted) = commands::restart(
                &hangar,
                bay.bay.as_deref(),
                &names,
                !no_copy,
            )?;
            Outcome::Restarted(bay, restarted)
        }
        Command::VaultUi => Outcome::VaultLogin(commands::vault_ui(&hangar)?),
        Command::Credential(action) => match action {
            CredentialCommand::Set { name } => {
                let value = credential::read(&hangar, &name)?;
                let _lock = hold_lock()?;
                done(credential::store(&hangar, &name, &value))?
            }
            CredentialCommand::List => {
                Outcome::Credentials(credential::list(&hangar)?)
            }
            CredentialCommand::Rm { name } => {
                done(credential::remove(&hangar, &name))?
            }
        },
        Command::Destroy { bays, state, yes } => {
            done(commands::destroy(&hangar, &bays, state, yes))?
        }
    })
}

impl Command {
    /// `credential set` locks itself, after its hidden prompt.
    fn changes_state(&self) -> bool {
        matches!(
            self,
            Self::Up { .. }
                | Self::Down { .. }
                | Self::Restart { .. }
                | Self::Copy { .. }
                | Self::Destroy { .. }
                | Self::Credential(CredentialCommand::Rm { .. })
        )
    }
}

fn hold_lock() -> Result<lock::Lock> {
    let path = dirs::hangar_dir(dirs::Base::State, &env_var)?.join("lock");
    lock::acquire(&path, || log::info!("waiting for another hangar"))
}

fn done(result: Result<()>) -> Result<Outcome> {
    result.map(|()| Outcome::Done)
}

fn streamed(result: Result<()>) -> Result<Outcome> {
    result.map(|()| Outcome::Streamed)
}
