//! Branch-only opt-in entry point for the existing incremental backfill job.
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use clap::{Parser, Subcommand};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use stax_server::services::durable_backfill::{BackfillQueue, SCHEMA};
use stax_server::{AppState, Config};

#[derive(Debug, Parser)]
#[command(about = "Schedule and run incremental ETL backfills with Rigwright durable work")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install only the pinned queue schema. Run once on a dedicated database/schema.
    Init,
    /// Persist a request; use a fresh request-id for each desired refresh.
    Enqueue {
        #[arg(long)]
        store_id: String,
        #[arg(long)]
        request_id: String,
    },
    /// Run queued work for an explicitly selected local store.
    Work {
        #[arg(long)]
        store_id: String,
        #[arg(long)]
        data_dir: PathBuf,
        /// Process at most one request, then exit.
        #[arg(long)]
        once: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let url = std::env::var("STAX_DURABLE_WORK_DATABASE_URL")
        .map_err(|_| anyhow!("set STAX_DURABLE_WORK_DATABASE_URL in the environment"))?;
    let options = url
        .parse::<PgConnectOptions>()
        .map_err(|_| anyhow!("invalid durable-work database URL"))?
        .options([
            ("search_path", "staxtrace_durable_work"),
            ("timezone", "UTC"),
        ]);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await
        .map_err(|_| anyhow!("could not connect to the durable-work database"))?;
    match cli.command {
        Command::Init => {
            let mut tx = pool.begin().await?;
            sqlx::query("CREATE SCHEMA IF NOT EXISTS staxtrace_durable_work")
                .execute(&mut *tx)
                .await?;
            sqlx::raw_sql(SCHEMA).execute(&mut *tx).await?;
            tx.commit().await?;
            println!("installed deskroost-jobs 0.1.0 queue schema");
        }
        Command::Enqueue {
            store_id,
            request_id,
        } => {
            BackfillQueue::new(pool.clone(), store_id)?
                .submit(&request_id)
                .await?;
            println!("incremental backfill request persisted");
        }
        Command::Work {
            store_id,
            data_dir,
            once,
        } => {
            let store = data_dir.join("store.db");
            ensure!(
                store.is_file(),
                "data-dir must contain an initialized store.db"
            );
            let state = AppState::new(store, data_dir, Config::default());
            let queue = BackfillQueue::new(pool.clone(), store_id)?;
            let worker = format!("staxtrace-backfill:{}", std::process::id());
            let mut shutdown = tokio::spawn(tokio::signal::ctrl_c());
            loop {
                // Do not abort spawn_blocking on shutdown: finish the current
                // attempt before honoring Ctrl-C between polling iterations.
                let result = queue.run_once(state.clone(), &worker).await?;
                println!("{result:?}");
                if once {
                    break;
                }
                tokio::select! {
                    _ = &mut shutdown => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
            shutdown.abort();
        }
    }
    pool.close().await;
    Ok(())
}
