//! The `arustex` command: one binary for the daemon, the terminal interface and
//! operator maintenance commands.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use arustex_app::client::Client;
use arustex_app::config::{self, CliOverrides, Loaded, ProcessEnv};
use arustex_app::daemon::{self, Daemon};
use arustex_app::ingest;
use arustex_core::CoreError;
use arustex_core::ipc::{Request, Response};
use arustex_store::Store;
use clap::{Args, Parser, Subcommand};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Parser)]
#[command(name = "arustex", version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Args)]
struct Global {
    /// Configuration file (default: $ARUSTEX_CONFIG, then the XDG and /etc locations).
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Daemon socket path (overrides the configuration).
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// Log level filter, such as `info` or `arustex=debug`.
    #[arg(long, global = true, value_name = "LEVEL")]
    log_level: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open the terminal interface (the default).
    Tui,
    /// Run the daemon in the foreground.
    Daemon,
    /// Inspect the configuration.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Database schema and backups.
    #[command(subcommand)]
    Db(DbCmd),
    /// Import records or legacy data.
    #[command(subcommand)]
    Import(ImportCmd),
    /// Check configuration, database, schema and daemon reachability.
    Doctor,
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    /// Parse and validate the configuration and resolve its secrets.
    Validate,
    /// Print the effective configuration with secrets redacted.
    Show,
}

#[derive(Debug, Subcommand)]
enum DbCmd {
    /// Apply pending schema migrations.
    Migrate,
    /// List migrations and whether they are applied.
    Status,
    /// Write a verified backup into an empty directory.
    Backup {
        #[arg(value_name = "DIR")]
        dir: PathBuf,
    },
    /// Check a backup's manifest and checksums without touching the database.
    VerifyBackup {
        #[arg(value_name = "DIR")]
        dir: PathBuf,
    },
    /// Restore a backup into an empty, migrated database (refuses existing data).
    Restore {
        #[arg(value_name = "DIR")]
        dir: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ImportCmd {
    /// Import a JSON Lines file of records into a dataset.
    Records(ImportRecords),
    /// Import allowlisted data from a legacy ARUSTEX PostgreSQL database (read-only).
    Legacy {
        /// Environment variable holding the legacy database URL.
        #[arg(
            long,
            default_value = "ARUSTEX_LEGACY_DATABASE_URL",
            value_name = "VAR"
        )]
        url_env: String,
        /// Count what would be imported without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Args)]
struct ImportRecords {
    /// The JSON Lines file (one object per line: external_id, title, content_type, body, fields).
    #[arg(long, value_name = "FILE")]
    file: PathBuf,
    /// Existing dataset id to import into.
    #[arg(long, conflicts_with = "new_dataset")]
    dataset: Option<i64>,
    /// Create a new dataset with this name.
    #[arg(long, value_name = "NAME", required_unless_present = "dataset")]
    new_dataset: Option<String>,
    /// Days until imported records may be purged (new datasets; default from config).
    #[arg(long)]
    retention_days: Option<u32>,
    /// Keep likely secrets (tokens, keys, passwords) instead of redacting them.
    #[arg(long)]
    no_redact: bool,
}

fn init_logging(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .try_init();
}

fn overrides(g: &Global) -> CliOverrides {
    CliOverrides {
        config_path: g.config.clone(),
        database_url: None,
        socket: g.socket.clone(),
        log_level: g.log_level.clone(),
    }
}

type CmdResult = Result<(), String>;

fn load(g: &Global, resolve_providers: bool) -> Result<Loaded, String> {
    config::load(&overrides(g), &ProcessEnv, resolve_providers).map_err(|e| e.to_string())
}

async fn store(loaded: &Loaded) -> Result<Store, String> {
    daemon::connect_store(loaded)
        .await
        .map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(dispatch(cli));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: Cli) -> CmdResult {
    let g = &cli.global;
    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => tui(g).await,
        Command::Daemon => run_daemon(g).await,
        Command::Config(ConfigCmd::Validate) => {
            let loaded = load(g, true)?;
            match &loaded.source {
                Some(p) => println!("configuration OK ({})", p.display()),
                None => println!("configuration OK (built-in defaults; no file found)"),
            }
            println!("providers: {}", loaded.config.providers.len());
            println!("socket: {}", loaded.socket.display());
            println!(
                "telegram: {}",
                if loaded.config.telegram.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            Ok(())
        }
        Command::Config(ConfigCmd::Show) => {
            let loaded = load(g, false)?;
            for (k, v) in loaded.config.redacted_entries() {
                println!("{k} = {v}");
            }
            Ok(())
        }
        Command::Db(cmd) => db(g, cmd).await,
        Command::Import(cmd) => import(g, cmd).await,
        Command::Doctor => doctor(g).await,
    }
}

async fn tui(g: &Global) -> CmdResult {
    let loaded = load(g, false)?;
    arustex_tui::run(arustex_tui::TuiOptions {
        socket: loaded.socket.clone(),
        request_timeout: Duration::from_secs(loaded.config.daemon.request_timeout_secs),
    })
    .await
    .map_err(|e| format!("terminal: {e}"))
}

async fn run_daemon(g: &Global) -> CmdResult {
    let loaded = load(g, true)?;
    init_logging(&loaded.config.logging.level);
    let shutdown = CancellationToken::new();
    daemon::install_signal_handlers(shutdown.clone()).map_err(|e| e.to_string())?;
    let d = Daemon::build(loaded, shutdown)
        .await
        .map_err(|e| e.to_string())?;
    d.run().await.map_err(|e| e.to_string())
}

async fn db(g: &Global, cmd: DbCmd) -> CmdResult {
    if let DbCmd::VerifyBackup { dir } = &cmd {
        let m = Store::verify_backup(dir).await.map_err(|e| e.to_string())?;
        println!(
            "backup OK: format {}, schema {}, {} tables, created {}",
            m.format,
            m.schema_version,
            m.tables.len(),
            m.created_at
        );
        return Ok(());
    }
    let loaded = load(g, false)?;
    init_logging(&loaded.config.logging.level);
    let store = store(&loaded).await?;
    let r = match cmd {
        DbCmd::Migrate => {
            store.migrate().await.map_err(|e| e.to_string())?;
            println!("schema is up to date");
            Ok(())
        }
        DbCmd::Status => {
            for m in store.migration_status().await.map_err(|e| e.to_string())? {
                println!(
                    "{:>4} {:<8} {}",
                    m.version,
                    if m.applied { "applied" } else { "pending" },
                    m.description
                );
            }
            Ok(())
        }
        DbCmd::Backup { dir } => {
            let m = store.backup_to(&dir).await.map_err(|e| e.to_string())?;
            println!(
                "backup written to {} ({} tables, schema {})",
                dir.display(),
                m.tables.len(),
                m.schema_version
            );
            Ok(())
        }
        DbCmd::Restore { dir } => {
            store.ensure_migrated().await.map_err(|e| e.to_string())?;
            let m = store.restore_from(&dir).await.map_err(|e| e.to_string())?;
            println!("restored {} tables from {}", m.tables.len(), dir.display());
            Ok(())
        }
        DbCmd::VerifyBackup { .. } => unreachable!("handled above"),
    };
    store.close().await;
    r
}

async fn import(g: &Global, cmd: ImportCmd) -> CmdResult {
    let loaded = load(g, false)?;
    init_logging(&loaded.config.logging.level);
    let store = store(&loaded).await?;
    store.ensure_migrated().await.map_err(|e| e.to_string())?;
    let actor = "operator:cli";
    let r = match cmd {
        ImportCmd::Records(args) => {
            let source = args
                .file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "import".into());
            let dataset = match (args.dataset, &args.new_dataset) {
                (Some(id), _) => arustex_core::domain::DatasetId(id),
                (None, Some(name)) => {
                    let retention = args
                        .retention_days
                        .unwrap_or(loaded.config.limits.record_retention_days);
                    store
                        .create_dataset(name, "", &source, retention, actor)
                        .await
                        .map_err(|e| e.to_string())?
                        .id
                }
                (None, None) => return Err("give --dataset or --new-dataset".into()),
            };
            let options = daemon::ingest_options(&loaded.config, &source, !args.no_redact);
            let report = ingest::import_jsonl_file(&store, dataset, &args.file, &options)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "dataset {dataset}: {} inserted, {} updated, {} duplicates, {} redacted",
                report.inserted, report.updated, report.duplicates, report.redacted
            );
            Ok(())
        }
        ImportCmd::Legacy { url_env, dry_run } => {
            let url = std::env::var(&url_env)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("set {url_env} to the legacy database URL"))?;
            let report = store
                .import_legacy(&url, dry_run, actor)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).unwrap_or_default()
            );
            Ok(())
        }
    };
    store.close().await;
    r
}

async fn doctor(g: &Global) -> CmdResult {
    let mut failed = false;
    let mut check = |name: &str, result: Result<String, String>| match result {
        Ok(detail) => println!("ok    {name}: {detail}"),
        Err(e) => {
            failed = true;
            println!("FAIL  {name}: {e}");
        }
    };
    let loaded = match load(g, true) {
        Ok(l) => {
            check(
                "configuration",
                Ok(l.source
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "built-in defaults".into())),
            );
            l
        }
        Err(e) => {
            check("configuration", Err(e));
            return Err("configuration is invalid".into());
        }
    };
    check(
        "providers",
        daemon::build_router(&loaded)
            .map(|r| r.names().collect::<Vec<_>>().join(", "))
            .map_err(|e| e.to_string()),
    );
    check(
        "telegram",
        Ok(if loaded.config.telegram.enabled {
            "enabled".into()
        } else {
            "disabled".into()
        }),
    );
    match store(&loaded).await {
        Ok(store) => {
            check(
                "database",
                store
                    .ping()
                    .await
                    .map(|()| "reachable".into())
                    .map_err(|e| e.to_string()),
            );
            check(
                "schema",
                store
                    .ensure_migrated()
                    .await
                    .map(|()| "up to date".into())
                    .map_err(|e| e.to_string()),
            );
            store.close().await;
        }
        Err(e) => check("database", Err(e)),
    }
    let daemon = match Client::connect(&loaded.socket, "arustex-doctor", Duration::from_secs(3), 16)
        .await
    {
        Ok((client, _events)) => match client.request(Request::Ping).await {
            Ok(Response::Pong) => Ok(format!("{} at {}", client.server, loaded.socket.display())),
            Ok(other) => Err(format!("unexpected reply {other:?}")),
            Err(e) => Err(e.to_string()),
        },
        Err(CoreError::Unavailable(m)) => Err(format!("not running ({m})")),
        Err(e) => Err(e.to_string()),
    };
    check("daemon", daemon);
    if failed {
        Err("some checks failed".into())
    } else {
        Ok(())
    }
}
