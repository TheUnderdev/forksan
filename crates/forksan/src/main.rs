mod client;
mod commands;
mod hook;

use clap::{Parser, Subcommand};
use forksan_core::config::Paths;

#[derive(Parser)]
#[command(
    name = "forksan",
    version,
    about = "Forks for Claude Code: throwaway forked-context runs at lifecycle moments"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Claude Code hook entrypoint (reads the hook JSON on stdin).
    #[command(hide = true)]
    Hook {
        #[arg(value_enum)]
        event: hook::HookKind,
    },
    /// Daemon, session, and fork-run status.
    Status,
    /// List the forks visible from the current (or given) directory.
    Forks {
        /// Project directory (defaults to the current directory).
        #[arg(long)]
        project: Option<std::path::PathBuf>,
    },
    /// Manually fire forks against the project's current session: one by
    /// name, or every fork carrying `--tag`.
    Run {
        /// Fork name (omit when using --tag).
        name: Option<String>,
        /// Run every fork carrying this tag instead of one by name.
        #[arg(long, conflicts_with = "name")]
        tag: Option<String>,
        /// Target session id (defaults to the project's most recent open one).
        #[arg(long)]
        session: Option<String>,
    },
    /// Show the daemon log.
    Logs {
        /// Keep following the log.
        #[arg(short, long)]
        follow: bool,
    },
    /// Check the installation and report problems.
    Doctor {
        /// Also delete our fork-session transcripts older than this age
        /// (e.g. 30d).
        #[arg(long, value_name = "AGE")]
        gc_fork_sessions: Option<String>,
    },
    /// Ask the daemon to exit (it restarts on the next hook event).
    StopDaemon {
        /// Wait for in-flight fork runs to finish first.
        #[arg(long, default_value_t = true)]
        drain: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let Some(paths) = Paths::from_env() else {
        eprintln!("forksan: cannot determine home directory");
        std::process::exit(1);
    };

    match cli.command {
        Command::Hook { event } => hook::run_hook(event),
        Command::Status => exit_on_err(commands::status(&paths)),
        Command::Forks { project } => exit_on_err(commands::list_forks(&paths, project)),
        Command::Run { name, tag, session } => {
            exit_on_err(commands::run_fork(&paths, name, tag, session))
        }
        Command::Logs { follow } => exit_on_err(commands::logs(&paths, follow)),
        Command::Doctor { gc_fork_sessions } => {
            exit_on_err(commands::doctor(&paths, gc_fork_sessions))
        }
        Command::StopDaemon { drain } => exit_on_err(stop_daemon(&paths, drain)),
    }
}

fn stop_daemon(paths: &Paths, drain: bool) -> Result<(), String> {
    use forksan_core::protocol::RequestBody;
    match client::Client::connect(paths, std::time::Duration::from_secs(5)) {
        Ok(mut c) => {
            let _ = c
                .request(RequestBody::Shutdown { drain })
                .map_err(|e| e.to_string())?;
            println!("daemon asked to exit");
            Ok(())
        }
        Err(_) => {
            println!("daemon not running");
            Ok(())
        }
    }
}

fn exit_on_err(result: Result<(), String>) {
    if let Err(e) = result {
        eprintln!("forksan: {e}");
        std::process::exit(1);
    }
}
