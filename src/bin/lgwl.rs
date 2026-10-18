//! lgwl: manage one background lgw serve process.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use light_ai_gateway::launcher::{
    ExitOutcome, LaunchOptions, LaunchOutcome, Launcher, Status, UpdateOptions, console_url,
    follow_log, read_tail,
};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "lgwl",
    version,
    about = "Launch, stop and update the background lgw gateway"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start lgw serve in the background, detached from this terminal.
    Launch {
        /// Path to the TOML configuration file (same resolution as lgw when omitted).
        #[arg(long, short = 'c', value_name = "PATH")]
        config: Option<PathBuf>,
    },
    /// Stop the background gateway.
    Exit,
    /// Show the gateway log; follows until Ctrl-C unless --no-follow is given.
    Log {
        /// How many existing lines to show before following.
        #[arg(short = 'n', long, default_value_t = 50, value_name = "LINES")]
        lines: usize,
        /// Print the current log and exit instead of following.
        #[arg(long)]
        no_follow: bool,
    },
    /// Reinstall lgw from its repository, restarting it if it was running.
    Update {
        /// Repository root; discovered from the install record or the working tree otherwise.
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
        /// Install but leave the gateway stopped.
        #[arg(long)]
        no_restart: bool,
    },
    /// Show whether the background gateway is running.
    Status,
}

fn print_urls(launcher: &Launcher, wait: Duration, fallback: bool) {
    match console_url(launcher.paths(), wait) {
        Some(base) => {
            println!("api:     {base}/v1");
            println!("console: {base}/");
        }
        None if fallback => println!("api:     not reported yet; see lgwl log"),
        None => {}
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let launcher = Launcher::discover()?;
    match cli.command {
        Command::Launch { config } => {
            let cwd = std::env::current_dir()?;
            match launcher.launch(&LaunchOptions::new(config, cwd))? {
                LaunchOutcome::Started(state) => {
                    println!("started lgw (pid {})", state.pid);
                    print_urls(&launcher, Duration::from_secs(5), true);
                    println!("log: {}", launcher.paths().log.display());
                    println!("follow it with: lgwl log");
                }
                LaunchOutcome::AlreadyRunning(state) => {
                    print_urls(&launcher, Duration::from_secs(1), false);
                    bail!("lgw is already running (pid {})", state.pid);
                }
            }
        }
        Command::Exit => match launcher.exit()? {
            ExitOutcome::Stopped { pid, forced } => {
                if forced {
                    println!("stopped lgw (pid {pid}) with SIGKILL after it ignored SIGTERM");
                } else {
                    println!("stopped lgw (pid {pid})");
                }
            }
            ExitOutcome::NotRunning => println!("lgw is not running"),
        },
        Command::Log { lines, no_follow } => {
            let path = launcher.paths().log.clone();
            if !path.is_file() {
                bail!("no log at {} yet; run lgwl launch first", path.display());
            }
            for line in read_tail(&path, lines)? {
                println!("{line}");
            }
            if !no_follow {
                follow_log(&path)?;
            }
        }
        Command::Update { repo, no_restart } => {
            let outcome = launcher.update(&UpdateOptions { repo, no_restart })?;
            println!("updated from {}", outcome.repo.display());
            if outcome.restarted {
                println!("lgw was restarted with its previous arguments");
                print_urls(&launcher, Duration::from_secs(5), false);
            } else if outcome.was_running {
                println!("lgw was stopped for the update; start it with: lgwl launch");
            } else {
                println!("lgw was not running");
            }
        }
        Command::Status => match launcher.status()? {
            Status::Running(state) => {
                println!("running (pid {})", state.pid);
                println!(
                    "started: {}",
                    time::OffsetDateTime::from_unix_timestamp(state.started_at)
                        .ok()
                        .and_then(|value| {
                            value
                                .format(&time::format_description::well_known::Rfc3339)
                                .ok()
                        })
                        .unwrap_or_else(|| format!("unix {}", state.started_at))
                );
                print_urls(&launcher, Duration::from_secs(1), false);
                println!("args: {:?}", state.args);
                println!("cwd: {}", state.cwd.display());
                println!("log: {}", launcher.paths().log.display());
            }
            Status::Stopped { stale } => {
                if stale {
                    println!("not running (the previous launch record is stale)");
                } else {
                    println!("not running");
                }
            }
        },
    }
    Ok(())
}
