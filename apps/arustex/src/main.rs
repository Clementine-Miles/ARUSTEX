#![forbid(unsafe_code)]

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
compile_error!("ARUSTEX supports Linux x86_64 and aarch64");

use std::{error::Error, io, path::PathBuf, process::ExitCode};

use arustex_app::{config::Config, ipc};
use arustex_core::{Query, Reply};
use arustex_store::Store;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about = "Read-only inspection of ARUSTEX records")]
struct Args {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve read-only record queries over a local Unix socket
    Daemon,
    /// Browse records in a Ratatui terminal client
    Tui,
    /// Check whether the local daemon responds
    Doctor,
    /// Check TOML configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    Validate,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("arustex: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let config = Config::load(args.config.as_deref())?;
    match args.command {
        Command::Daemon => {
            let store = Store::connect(config.database_dsn()?, config.max_connections).await?;
            ipc::serve(&config.socket, store).await?;
        }
        Command::Tui => {
            arustex_tui::run(|query| {
                let socket = config.socket.clone();
                async move {
                    ipc::request(&socket, query)
                        .await
                        .map_err(|error| error.to_string())
                }
            })
            .await?;
        }
        Command::Doctor => {
            if ipc::request(&config.socket, Query::Ping).await? != Reply::Pong {
                return Err(io::Error::other("daemon returned an unexpected response").into());
            }
            println!("Local daemon is responding.");
        }
        Command::Config {
            command: ConfigCommand::Validate,
        } => println!("TOML configuration is valid."),
    }
    Ok(())
}
