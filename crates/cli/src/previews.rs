use std::sync::Arc;

use anyhow::{Context, Result};
use executor::{AgentHandle, ConversationHandle, PreviewUrls, Runtime};

pub(crate) fn print_startup(previews: &PreviewUrls) {
    println!("sandbox: {}", previews.page);
    println!("  Open this page for service links. Services must be running in the sandbox.");
    for service in &previews.services {
        println!("  port {}: {}", service.port, service.url);
    }
}

pub(crate) async fn print(
    runtime: &Runtime,
    agent: &dyn AgentHandle,
    thread: Arc<dyn ConversationHandle>,
) -> Result<()> {
    let previews = runtime.preview_urls(agent, thread).await?.context(
        "no browser previews are available; declare config.tcp_ports in the environment and run this thread",
    )?;
    println!("sandbox: {}", previews.page);
    crate::print_table(
        &["PORT", "BROWSER URL"],
        previews
            .services
            .into_iter()
            .map(|service| vec![service.port.to_string(), service.url])
            .collect(),
    )?;
    println!("Preview links require the owning Exo process and sandbox services to be running.");
    Ok(())
}
