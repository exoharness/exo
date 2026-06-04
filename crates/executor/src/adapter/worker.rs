use std::path::Path;

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Notify, mpsc};

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
    outbound_notify: Arc<Notify>,
    on_event: F,
    take_outbound_messages: G,
    mut should_stop: S,
) -> Result<()>
where
    F: FnMut(WorkerEvent) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    G: FnMut() -> OutFut + Send + 'static,
    OutFut: std::future::Future<Output = Result<Vec<WorkerCommand>>> + Send + 'static,
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
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("adapter worker stdin was not piped"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("adapter worker stdout was not piped"))?;
    let mut lines = BufReader::new(stdout).lines();
    let mut stop_interval = tokio::time::interval(std::time::Duration::from_secs(1));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut event_task = tokio::spawn(process_worker_events(event_rx, on_event));
    let mut command_task = tokio::spawn(process_worker_commands(
        stdin,
        outbound_notify,
        take_outbound_messages,
    ));

    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                command_task.abort();
                event_task.abort();
                bail!("adapter worker exited with status {status}");
            }
            line = lines.next_line() => {
                let Some(line) = line? else {
                    command_task.abort();
                    event_task.abort();
                    bail!("adapter worker closed stdout");
                };
                let event = match serde_json::from_str::<WorkerEvent>(&line) {
                    Ok(event) => event,
                    Err(error) => {
                        command_task.abort();
                        event_task.abort();
                        return Err(error)
                            .with_context(|| format!("failed to parse adapter worker event: {line}"));
                    }
                };
                tracing::debug!(
                    adapter_type = %config.adapter_type,
                    event = ?event,
                    "adapter worker event"
                );
                if event_tx.send(event).is_err() {
                    command_task.abort();
                    event_task.abort();
                    bail!("adapter event handler stopped");
                }
            }
            _ = stop_interval.tick() => {
                if should_stop().await? {
                    command_task.abort();
                    event_task.abort();
                    return Ok(());
                }
            }
            result = &mut event_task => {
                command_task.abort();
                match result {
                    Ok(Ok(())) => bail!("adapter event handler stopped"),
                    Ok(Err(error)) => return Err(error),
                    Err(error) => return Err(anyhow!("adapter event handler task failed: {error}")),
                }
            }
            result = &mut command_task => {
                event_task.abort();
                match result {
                    Ok(Ok(())) => bail!("adapter command sender stopped"),
                    Ok(Err(error)) => return Err(error),
                    Err(error) => return Err(anyhow!("adapter command sender task failed: {error}")),
                }
            }
        }
    }
}

async fn process_worker_events<F, Fut>(
    mut event_rx: mpsc::UnboundedReceiver<WorkerEvent>,
    mut on_event: F,
) -> Result<()>
where
    F: FnMut(WorkerEvent) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    while let Some(event) = event_rx.recv().await {
        on_event(event).await?;
    }
    Ok(())
}

async fn process_worker_commands<G, OutFut>(
    mut stdin: ChildStdin,
    outbound_notify: Arc<Notify>,
    mut take_outbound_messages: G,
) -> Result<()>
where
    G: FnMut() -> OutFut,
    OutFut: std::future::Future<Output = Result<Vec<WorkerCommand>>>,
{
    loop {
        send_pending_commands(&mut stdin, &outbound_notify, &mut take_outbound_messages).await?;
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
    outbound_notify: &Notify,
    take_outbound_messages: &mut G,
) -> Result<()>
where
    G: FnMut() -> OutFut,
    OutFut: std::future::Future<Output = Result<Vec<WorkerCommand>>>,
{
    tokio::select! {
        () = outbound_notify.notified() => {}
        () = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
    }
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
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::{Notify, oneshot};

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
            initialization: json!({}),
            state_dir: None,
            secret_env: Vec::new(),
        };
        let outbound_notify = Arc::new(Notify::new());

        run_worker_loop(
            "adapter",
            &config,
            Vec::new(),
            outbound_notify,
            |_event| async { Ok(()) },
            || async { Ok(Vec::new()) },
            || async { Ok(true) },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn dispatches_outbound_commands_while_event_handler_is_busy() {
        let tempdir = TempDir::new().unwrap();
        let output_path = tempdir.path().join("command.json");
        let config = AdapterConfig {
            adapter_type: "test".to_string(),
            worker_command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "printf '%s\n' '{\"type\":\"message\",\"target\":\"target\",\"text\":\"hello\"}'; IFS= read -r line; printf '%s\n' \"$line\" > \"$OUTPUT_PATH\"; sleep 60".to_string(),
            ],
            initialization: json!({}),
            state_dir: None,
            secret_env: Vec::new(),
        };
        let outbound = Arc::new(Mutex::new(Vec::<WorkerCommand>::new()));
        let outbound_for_worker = Arc::clone(&outbound);
        let outbound_notify = Arc::new(Notify::new());
        let outbound_notify_for_worker = Arc::clone(&outbound_notify);
        let (event_started_tx, event_started_rx) = oneshot::channel();
        let (release_event_tx, release_event_rx) = oneshot::channel();
        let mut event_started_tx = Some(event_started_tx);
        let mut release_event_rx = Some(release_event_rx);

        let worker = tokio::spawn(async move {
            run_worker_loop(
                "adapter",
                &config,
                vec![(
                    "OUTPUT_PATH".to_string(),
                    output_path.to_string_lossy().into_owned(),
                )],
                outbound_notify_for_worker,
                move |event| {
                    let event_started_tx = event_started_tx.take();
                    let release_event_rx = release_event_rx.take();
                    async move {
                        if matches!(event, WorkerEvent::Message { .. }) {
                            if let Some(event_started_tx) = event_started_tx
                                && event_started_tx.send(()).is_err()
                            {
                                panic!("test receiver dropped before event started");
                            }
                            if let Some(release_event_rx) = release_event_rx {
                                release_event_rx.await.unwrap();
                            }
                        }
                        Ok(())
                    }
                },
                move || {
                    let outbound = Arc::clone(&outbound_for_worker);
                    async move {
                        let mut outbound = outbound.lock().unwrap();
                        Ok(outbound.drain(..).collect())
                    }
                },
                || async { Ok(false) },
            )
            .await
        });

        event_started_rx.await.unwrap();
        outbound.lock().unwrap().push(WorkerCommand::SendMessage {
            id: "command".to_string(),
            target: Some("target".to_string()),
            text: "pong".to_string(),
            attachments: Vec::new(),
        });
        outbound_notify.notify_one();

        let written = wait_for_file(tempdir.path().join("command.json")).await;
        assert!(written.contains("\"type\":\"send_message\""));
        assert!(written.contains("\"text\":\"pong\""));

        release_event_tx.send(()).unwrap();
        worker.abort();
        match worker.await {
            Err(error) if error.is_cancelled() => {}
            other => panic!("worker task should have been cancelled, got {other:?}"),
        }
    }

    async fn wait_for_file(path: impl AsRef<Path>) -> String {
        let path = path.as_ref().to_path_buf();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&path).await {
                    return contents;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("worker did not receive outbound command")
    }
}
