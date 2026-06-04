use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};

use super::types::{AdapterAttachment, AdapterConfig};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerCommand {
    SendMessage {
        id: String,
        #[serde(default)]
        target: Option<String>,
        text: String,
        #[serde(default)]
        attachments: Vec<AdapterAttachment>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    Connected {
        #[serde(default)]
        subject: Option<String>,
        #[serde(default)]
        metadata: Value,
    },
    Message {
        target: String,
        #[serde(default)]
        sender: Option<String>,
        text: String,
        #[serde(default)]
        message_id: Option<String>,
        #[serde(default)]
        metadata: Value,
    },
    Lifecycle {
        name: String,
        #[serde(default)]
        metadata: Value,
    },
    Error {
        message: String,
    },
    CommandAck {
        command_id: String,
    },
    CommandNack {
        command_id: String,
        message: String,
    },
    Disconnected {
        #[serde(default)]
        reason: Option<String>,
    },
}

pub async fn run_worker_loop<F, Fut, G, OutFut, S, StopFut>(
    adapter_id: &str,
    config: &AdapterConfig,
    secret_env: Vec<(String, String)>,
    mut on_event: F,
    mut take_outbound_messages: G,
    mut should_stop: S,
) -> Result<()>
where
    F: FnMut(WorkerEvent) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
    G: FnMut() -> OutFut,
    OutFut: std::future::Future<Output = Result<Vec<WorkerCommand>>>,
    S: FnMut() -> StopFut,
    StopFut: std::future::Future<Output = Result<bool>>,
{
    tracing::info!(
        adapter_type = %config.adapter_type,
        adapter_id = %adapter_id,
        worker_command = ?config.worker_command,
        "starting adapter worker"
    );
    let mut command = worker_command(adapter_id, config, secret_env);
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn adapter worker")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("adapter worker stdin was not piped"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("adapter worker stdout was not piped"))?;
    let mut lines = BufReader::new(stdout).lines();
    let mut outbound_interval = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut stop_interval = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                bail!("adapter worker exited with status {status}");
            }
            line = lines.next_line() => {
                let Some(line) = line? else {
                    bail!("adapter worker closed stdout");
                };
                let event = serde_json::from_str::<WorkerEvent>(&line)
                    .with_context(|| format!("failed to parse adapter worker event: {line}"))?;
                tracing::debug!(
                    adapter_type = %config.adapter_type,
                    event = ?event,
                    "adapter worker event"
                );
                on_event(event).await?;
            }
            _ = outbound_interval.tick() => {
                send_pending_commands(&mut stdin, &mut take_outbound_messages).await?;
            }
            _ = stop_interval.tick() => {
                if should_stop().await? {
                    return Ok(());
                }
            }
        }
    }
}

fn worker_command(
    adapter_id: &str,
    config: &AdapterConfig,
    secret_env: Vec<(String, String)>,
) -> Command {
    let args = &config.worker_command;
    let mut command = Command::new(&args[0]);
    command.args(&args[1..]);
    command.env("EXO_ADAPTER_ID", adapter_id);
    command.env("EXO_ADAPTER_TYPE", &config.adapter_type);
    command.env("EXO_ADAPTER_STATE_DIR", state_dir(adapter_id, config));
    command.env(
        "EXO_ADAPTER_CONFIG",
        serde_json::to_string(&config.initialization).expect("adapter initialization is JSON"),
    );
    for (name, value) in secret_env {
        command.env(name, value);
    }
    command
}

fn state_dir(adapter_id: &str, config: &AdapterConfig) -> String {
    config.state_dir.clone().unwrap_or_else(|| {
        Path::new(".exo")
            .join("adapters")
            .join(&config.adapter_type)
            .join(adapter_id)
            .to_string_lossy()
            .to_string()
    })
}

async fn send_pending_commands<G, OutFut>(
    stdin: &mut ChildStdin,
    take_outbound_messages: &mut G,
) -> Result<()>
where
    G: FnMut() -> OutFut,
    OutFut: std::future::Future<Output = Result<Vec<WorkerCommand>>>,
{
    for command in take_outbound_messages().await? {
        tracing::debug!(command = ?command, "sending adapter worker command");
        stdin
            .write_all(serde_json::to_string(&command)?.as_bytes())
            .await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stops_worker_when_cancellation_trips() {
        let config = AdapterConfig {
            adapter_type: "test".to_string(),
            worker_command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "while true; do sleep 1; done".to_string(),
            ],
            initialization: serde_json::json!({}),
            state_dir: None,
            secret_env: Vec::new(),
        };

        run_worker_loop(
            "adapter",
            &config,
            Vec::new(),
            |_event| async { Ok(()) },
            || async { Ok(Vec::new()) },
            || async { Ok(true) },
        )
        .await
        .unwrap();
    }
}
