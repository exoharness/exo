use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result, anyhow, ensure};
use executor::{ConversationHandle, EventData, EventKind, EventQuery, EventQueryDirection};
use tokio::{io::copy_bidirectional, net::TcpListener, task::JoinSet};

pub(crate) async fn run(
    conversation: Arc<dyn ConversationHandle>,
    port: u16,
    bind: SocketAddr,
) -> Result<()> {
    let running = conversation.list_sandboxes().await?;
    let events = conversation
        .get_events(Some(EventQuery {
            direction: Some(EventQueryDirection::Desc),
            types: Some(vec![EventKind::SANDBOX_CREATED]),
            ..Default::default()
        }))
        .await?;
    let sandbox_id = events
        .events
        .into_iter()
        .find_map(|event| match event.data {
            EventData::SandboxCreated {
                sandbox_id,
                tcp_ports,
                ..
            } if tcp_ports.contains(&port)
                && running
                    .iter()
                    .any(|sandbox| sandbox.id == sandbox_id && sandbox.running) =>
            {
                Some(sandbox_id)
            }
            _ => None,
        })
        .ok_or_else(|| anyhow!("no running thread sandbox publishes TCP port {port}"))?;
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
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (mut client, _) = result?;
                let conversation = Arc::clone(&conversation);
                let sandbox_id = sandbox_id.clone();
                connections.spawn(async move {
                    let mut upstream = conversation
                        .connect_sandbox_tcp(sandbox_id, port)
                        .await?
                        .context("sandbox provider did not return a TCP connection")?;
                    copy_bidirectional(&mut client, &mut upstream).await?;
                    Ok::<(), anyhow::Error>(())
                });
            }
            result = connections.join_next(), if !connections.is_empty() => {
                match result {
                    Some(Ok(Err(error))) => eprintln!("port forward connection failed: {error:#}"),
                    Some(Err(error)) => eprintln!("port forward task failed: {error}"),
                    _ => {}
                }
            }
            result = tokio::signal::ctrl_c() => {
                result?;
                return Ok(());
            }
        }
    }
}
