use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};

use crate::adapter_types::WhatsappAdapterConfig;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerCommand {
    SendMessage { target: String, text: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    Qr {
        qr: String,
    },
    Connected {
        jid: Option<String>,
    },
    Message {
        chat_id: String,
        sender: Option<String>,
        text: String,
        message_id: Option<String>,
    },
    Error {
        message: String,
    },
    Disconnected {
        reason: Option<String>,
    },
}

pub async fn run_whatsapp_worker_loop<F, Fut, G, OutFut>(
    adapter_id: &str,
    config: &WhatsappAdapterConfig,
    mut on_event: F,
    mut take_outbound_messages: G,
) -> Result<()>
where
    F: FnMut(WorkerEvent) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
    G: FnMut() -> OutFut,
    OutFut: std::future::Future<Output = Result<Vec<WorkerCommand>>>,
{
    let mut command = worker_command(adapter_id, config);
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("failed to spawn WhatsApp adapter worker")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("WhatsApp worker stdin was not piped"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("WhatsApp worker stdout was not piped"))?;
    let mut lines = BufReader::new(stdout).lines();

    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                bail!("WhatsApp worker exited with status {status}");
            }
            line = lines.next_line() => {
                let Some(line) = line? else {
                    bail!("WhatsApp worker closed stdout");
                };
                let event = serde_json::from_str::<WorkerEvent>(&line)
                    .with_context(|| format!("failed to parse WhatsApp worker event: {line}"))?;
                on_event(event).await?;
            }
            result = send_pending_commands(&mut stdin, &mut take_outbound_messages) => {
                result?;
            }
        }
    }
}

fn worker_command(adapter_id: &str, config: &WhatsappAdapterConfig) -> Command {
    let args = config.worker_command.clone().unwrap_or_else(|| {
        vec![
            "pnpm".to_string(),
            "tsx".to_string(),
            "examples/exoclaw/adapters/whatsapp/worker.ts".to_string(),
        ]
    });
    let mut command = Command::new(&args[0]);
    command.args(&args[1..]);
    command.env("EXO_ADAPTER_ID", adapter_id);
    command.env("EXO_WHATSAPP_AUTH_DIR", auth_dir(adapter_id, config));
    command
}

fn auth_dir(adapter_id: &str, config: &WhatsappAdapterConfig) -> String {
    config.auth_dir.clone().unwrap_or_else(|| {
        Path::new(".exo")
            .join("adapters")
            .join("whatsapp")
            .join(adapter_id)
            .join("auth")
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
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    for command in take_outbound_messages().await? {
        stdin
            .write_all(serde_json::to_string(&command)?.as_bytes())
            .await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
    }
    Ok(())
}
