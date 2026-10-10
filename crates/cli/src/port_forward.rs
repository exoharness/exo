use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result, ensure};
use executor::ConversationHandle;
pub(crate) use executor::previews::published_sandbox;
use tokio::{io::copy_bidirectional, net::TcpListener};

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

async fn forward(
    conversation: Arc<dyn ConversationHandle>,
    sandbox_id: executor::SandboxId,
    port: u16,
    listener: TcpListener,
) -> Result<()> {
    tokio::select! {
        result = executor::local_net::serve_connections(
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
