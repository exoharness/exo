use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Subcommand;
use executor::{Harness, SchedulerRunOptions, SchedulerStore, run_due_tasks};

#[derive(Debug, Subcommand)]
pub enum ScheduleCommands {
    List {
        #[arg(long)]
        include_disabled: bool,
    },
    Run {
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 60)]
        interval_seconds: u64,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    Cancel {
        task_id: String,
    },
    Delete {
        task_id: String,
    },
}

pub async fn handle_schedule_command(
    root: &Path,
    harness: Arc<dyn Harness>,
    command: ScheduleCommands,
) -> Result<()> {
    let store = SchedulerStore::new(root.join("scheduled-tasks"));
    match command {
        ScheduleCommands::List { include_disabled } => {
            println!("TASK\tENABLED\tSCHEDULE\tNEXT_RUN_AT_MS\tNAME");
            for task in store
                .list_tasks()
                .await?
                .into_iter()
                .filter(|task| include_disabled || task.enabled)
            {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    task.id, task.enabled, task.schedule, task.next_run_at_ms, task.name
                );
            }
        }
        ScheduleCommands::Run {
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
        ScheduleCommands::Cancel { task_id } => {
            if let Some(task) = store.disable_task(&task_id).await? {
                stop_task_owned_sandbox(harness.as_ref(), &task).await?;
                println!("cancelled scheduled task {}", task_id);
            } else {
                bail!("scheduled task not found: {task_id}");
            }
        }
        ScheduleCommands::Delete { task_id } => {
            if let Some(task) = store.delete_task(&task_id).await? {
                stop_task_owned_sandbox(harness.as_ref(), &task).await?;
                println!("deleted scheduled task {}", task_id);
            } else {
                bail!("scheduled task not found: {task_id}");
            }
        }
    }
    Ok(())
}

async fn stop_task_owned_sandbox(
    harness: &dyn Harness,
    task: &executor::ScheduledTaskRecord,
) -> Result<()> {
    let Some(sandbox_id) = task.task_sandbox_id.clone() else {
        return Ok(());
    };
    if let Some(agent) = harness.get_agent(&task.agent_id).await?
        && let Some(conversation) = agent.get_conversation(&task.conversation_id).await?
    {
        conversation
            .exoharness_handle()
            .stop_sandbox(sandbox_id)
            .await?;
    }
    Ok(())
}
