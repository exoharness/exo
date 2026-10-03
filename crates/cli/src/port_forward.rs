use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result, anyhow, ensure};
use executor::ConversationHandle;
use tokio::{io::copy_bidirectional, net::TcpListener};

#[cfg(test)]
mod tests;

pub(crate) async fn run(
    conversation: Arc<dyn ConversationHandle>,
    port: u16,
    bind: SocketAddr,
) -> Result<()> {
    let sandbox_id = published_sandbox(conversation.as_ref(), port).await?;
    ensure!(
        conversation
            .sandbox_supports_tcp(sandbox_id.clone())
            .await?,
        "sandbox provider does not support TCP connections"
    );
    let listener = TcpListener::bind(bind)
        .await
        .context("binding local port forward")?;
    println!(
        "Forwarding {} to {} guest port {} (Ctrl-C to stop)",
        listener.local_addr()?,
        conversation.record().slug,
        port
    );
    forward(conversation, sandbox_id, port, listener).await
}

pub(crate) async fn published_sandbox(
    conversation: &dyn ConversationHandle,
    port: u16,
) -> Result<executor::SandboxId> {
    conversation
        .list_sandboxes()
        .await?
        .into_iter()
        .find(|sandbox| sandbox.running && sandbox.tcp_ports.contains(&port))
        .map(|sandbox| sandbox.id)
        .ok_or_else(|| anyhow!("no running thread sandbox publishes TCP port {port}"))
}

async fn forward(
    conversation: Arc<dyn ConversationHandle>,
    sandbox_id: executor::SandboxId,
    port: u16,
    listener: TcpListener,
) -> Result<()> {
    tokio::select! {
        result = crate::local_net::serve_connections(
            || async { Ok(listener.accept().await?.0) },
            move |mut client| {
                let conversation = Arc::clone(&conversation);
                let sandbox_id = sandbox_id.clone();
                async move {
                    let mut upstream = conversation
                        .connect_sandbox_tcp(sandbox_id, port)
                        .await?
                        .context("sandbox provider did not return a TCP connection")?;
                    copy_bidirectional(&mut client, &mut upstream).await?;
                    Ok::<(), anyhow::Error>(())
                }
            },
            "port forward",
            |error| eprintln!("port forward connection failed: {error:#}"),
        ) => result,
        result = tokio::signal::ctrl_c() => {
            result?;
            Ok(())
        }
    }
}
