use crate::{
    client::{AuthError, Cancelled, Client},
    config::{self, Cluster, Config, Overrides},
    export::{self, Format},
};
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{fmt, path::PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Azure Data Explorer query CLI and terminal workspace"
)]
pub struct Args {
    #[arg(short = 'r', long, value_name = "FILE")]
    pub run: Option<PathBuf>,
    #[arg(short = 'c', long)]
    pub cluster: Option<String>,
    #[arg(short = 'd', long)]
    pub database: Option<String>,
    #[arg(long)]
    pub tenant: Option<String>,
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub timeout: Option<u64>,
    #[arg(long)]
    pub max_rows: Option<usize>,
    #[arg(long)]
    pub max_bytes: Option<usize>,
    #[arg(long, value_enum, default_value = "json")]
    pub format: Format,
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub force: bool,
    #[arg(long)]
    pub accept_partial: bool,
    /// Zero-based primary result table index. Required for multi-table exports.
    #[arg(long)]
    pub table: Option<usize>,
    #[command(subcommand)]
    pub command: Option<Commands>,
}
#[derive(Subcommand, Debug)]
pub enum Commands {
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Clusters {
        #[command(subcommand)]
        command: ClusterCommand,
    },
}
#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    Init,
}
#[derive(Subcommand, Debug)]
pub enum ClusterCommand {
    List,
    Add {
        alias: String,
        endpoint: String,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        tenant: Option<String>,
        #[arg(long)]
        default: bool,
    },
}

#[derive(Debug)]
pub struct Failure {
    pub code: u8,
    error: anyhow::Error,
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.error)
    }
}
fn failure(code: u8) -> impl FnOnce(anyhow::Error) -> Failure {
    move |error| Failure { code, error }
}

pub async fn run(args: Args) -> std::result::Result<(), Failure> {
    let path = args
        .config
        .clone()
        .map(Ok)
        .unwrap_or_else(config::config_path)
        .map_err(failure(2))?;
    let mut config = Config::load(&path).map_err(failure(2))?;
    if let Some(command) = args.command {
        return (|| -> Result<()> {
            ensure!(
                args.run.is_none(),
                "subcommands cannot be combined with --run"
            );
            match command {
                Commands::Config {
                    command: ConfigCommand::Init,
                } => {
                    config.save(&path, args.force)?;
                    eprintln!("config: {}", path.display());
                }
                Commands::Clusters {
                    command: ClusterCommand::List,
                } => {
                    for (name, c) in &config.clusters {
                        println!(
                            "{}\t{}\t{}",
                            crate::safe_text(name),
                            c.endpoint,
                            crate::safe_text(c.database.as_deref().unwrap_or("-"))
                        );
                    }
                }
                Commands::Clusters {
                    command:
                        ClusterCommand::Add {
                            alias,
                            endpoint,
                            database,
                            tenant,
                            default,
                        },
                } => {
                    ensure!(
                        !config.clusters.contains_key(&alias) || args.force,
                        "alias already exists; use --force to replace"
                    );
                    let endpoint = config::endpoint(&endpoint)?;
                    config.clusters.insert(
                        alias.clone(),
                        Cluster {
                            endpoint,
                            database,
                            tenant,
                            auth: "azure-cli".into(),
                            query_timeout_secs: None,
                        },
                    );
                    if default {
                        config.default_cluster = Some(alias);
                    }
                    config.save(&path, true)?;
                }
            }
            Ok(())
        })()
        .map_err(failure(2));
    }
    let overrides = Overrides {
        cluster: args.cluster,
        database: args.database,
        tenant: args.tenant,
        timeout: args.timeout,
        max_rows: args.max_rows,
        max_bytes: args.max_bytes,
    };
    if let Some(file) = args.run {
        let target = config.resolve(&overrides).map_err(failure(2))?;
        let text = tokio::fs::read_to_string(&file)
            .await
            .with_context(|| format!("reading {}", file.display()))
            .map_err(failure(6))?;
        let client = Client::new().map_err(failure(4))?;
        let id = Client::request_id();
        let cancel = CancellationToken::new();
        let result = tokio::select! {
            result = client.query(&target, &text, &id, cancel.clone()) => result,
            signal = tokio::signal::ctrl_c() => {
                signal.context("installing Ctrl-C handler").map_err(failure(4))?;
                cancel.cancel();
                match client.cancel_server(&target, &id).await {
                    Ok(()) => eprintln!("server cancellation request accepted; execution may already have completed"),
                    Err(e) => eprintln!("server cancellation unconfirmed: {}", crate::safe_text(&e.to_string())),
                }
                Err(Cancelled.into())
            }
        }.map_err(|error| {
            let code = if error.is::<AuthError>() { 3 } else if error.is::<Cancelled>() { 130 } else { 4 };
            Failure { code, error }
        })?;
        eprintln!(
            "request={} activity={}",
            result.client_request_id,
            result.activity_id.as_deref().unwrap_or("unavailable")
        );
        for diagnostic in &result.diagnostics {
            eprintln!("{}", crate::safe_text(diagnostic));
        }
        if result.partial && !args.accept_partial {
            return Err(failure(5)(anyhow::anyhow!(
                "PARTIAL results refused; use --accept-partial to export"
            )));
        }
        let table_index = args.table.unwrap_or(0);
        let table = result
            .tables
            .get(table_index)
            .context("selected primary result table does not exist")
            .map_err(failure(4))?;
        if result.tables.len() > 1 && args.table.is_none() {
            return Err(failure(2)(anyhow::anyhow!(
                "{} primary tables returned; select one explicitly with --table INDEX",
                result.tables.len()
            )));
        }
        let rows: Vec<usize> = (0..table.rows.len()).collect();
        if let Some(output) = args.output {
            export::file(
                &output,
                args.force,
                &result,
                table,
                &rows,
                args.format,
                args.accept_partial,
            )
            .map_err(failure(6))?;
        } else {
            export::write(
                std::io::stdout().lock(),
                &result,
                table,
                &rows,
                args.format,
                args.accept_partial,
            )
            .map_err(failure(6))?;
        }
        Ok(())
    } else {
        crate::tui::run(config, path, overrides)
            .await
            .map_err(failure(4))
    }
}
