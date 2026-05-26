use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Subcommand;
use executor::{Harness, SchedulerRunOptions, SchedulerStore, run_due_tasks};

#[derive(Debug, Subcommand)]
pub enum ExoclawCommands {
    Scheduler {
        #[command(subcommand)]
        command: ExoclawSchedulerCommands,
    },
}

#[derive(Debug, Subcommand)]
pub enum ExoclawSchedulerCommands {
    Run {
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 60)]
        interval_seconds: u64,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
}

pub async fn handle_exoclaw_command(
    root: &Path,
    harness: Arc<dyn Harness>,
    command: ExoclawCommands,
) -> Result<()> {
    match command {
        ExoclawCommands::Scheduler { command } => {
            handle_scheduler_command(root, harness, command).await
        }
    }
}

async fn handle_scheduler_command(
    root: &Path,
    harness: Arc<dyn Harness>,
    command: ExoclawSchedulerCommands,
) -> Result<()> {
    let store = SchedulerStore::new(root.join("scheduled-tasks"));
    match command {
        ExoclawSchedulerCommands::Run {
            watch,
            interval_seconds,
            limit,
        } => loop {
            let runs =
                run_due_tasks(Arc::clone(&harness), &store, SchedulerRunOptions { limit }).await?;
            for run in runs {
                println!(
                    "{}\t{}\texit={}\terror={}",
                    run.task_id,
                    run.id,
                    run.exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "none".to_string()),
                    run.error.unwrap_or_else(|| "none".to_string())
                );
            }
            if !watch {
                break;
            }
            tokio::time::sleep(Duration::from_secs(interval_seconds)).await;
        },
    }
    Ok(())
}
