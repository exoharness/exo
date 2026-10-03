use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use executor::{AgentHandle, BrowserPreview, ConversationHandle, Runtime};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{UnixListener, UnixStream},
    task::JoinHandle,
    time::timeout,
};

mod proxy;
pub(crate) use proxy::run as run_proxy;

pub(crate) struct PreviewSession {
    task: JoinHandle<()>,
    proxy: proxy::Client,
    _gateway: tempfile::TempDir,
}

impl Drop for PreviewSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl PreviewSession {
    pub(crate) async fn start(
        runtime: &Runtime,
        agent: &dyn AgentHandle,
        thread: Arc<dyn ConversationHandle>,
        root: impl FnOnce() -> Result<PathBuf>,
    ) -> Result<Option<Self>> {
        let mut config = runtime.get_conversation_config(thread.as_ref()).await?;
        let preview_config = config
            .environment
            .as_ref()
            .and_then(|environment| environment.previews.clone());
        let Some(preview_config) = preview_config else {
            if config.browser_preview_url.is_some() || !config.browser_previews.is_empty() {
                config.browser_preview_url = None;
                config.browser_previews.clear();
                runtime
                    .put_conversation_config(thread.as_ref(), config)
                    .await?;
            }
            return Ok(None);
        };
        let directory = root()?.join("previews");
        std::fs::create_dir_all(&directory)?;
        let proxy = proxy::Client::connect(&directory).await?;
        let port = proxy.port;
        let hostname = thread_hostname(
            &agent.record().slug,
            &thread.record().slug,
            &thread.record().id.to_string(),
            &preview_config.domain,
        )?;
        let mut services = preview_config.services;
        for port in &config
            .environment
            .as_ref()
            .context("preview environment is missing")?
            .config
            .tcp_ports
        {
            if !services.values().any(|service_port| service_port == port) {
                let name = port.to_string();
                ensure!(
                    !services.contains_key(&name),
                    "service name {name} conflicts with published port {port}; configure a service name for that port"
                );
                services.insert(name, *port);
            }
        }
        ensure!(
            services
                .keys()
                .all(|name| name.len() + hostname.len() < 253),
            "preview hostname exceeds the DNS length limit"
        );
        let services: Vec<_> = services
            .into_iter()
            .map(|(name, guest_port)| proxy::Service {
                host: format!("{name}.{hostname}:{port}"),
                name,
                port: guest_port,
            })
            .collect();
        let previews: Vec<_> = services
            .iter()
            .map(|service| BrowserPreview {
                url: format!("http://{}", service.host),
                name: service.name.clone(),
                port: service.port,
            })
            .collect();
        let index_url = format!("http://{hostname}:{port}");
        let changed = config.browser_preview_url.as_deref() != Some(index_url.as_str())
            || config.browser_previews != previews;
        config.browser_preview_url = Some(index_url.clone());
        config.browser_previews = previews.clone();
        let gateway = crate::local_net::socket_directory()?;
        let socket = gateway.path().join("gateway.sock");
        let listener = UnixListener::bind(&socket).context("binding preview session gateway")?;
        let ports = previews.iter().map(|preview| preview.port).collect();
        let task = tokio::spawn(serve_gateway(listener, thread.clone(), ports));
        let mut session = Self {
            task,
            proxy,
            _gateway: gateway,
        };
        session
            .proxy
            .register(proxy::Registration {
                gateway: socket,
                portal: format!("{hostname}:{port}"),
                services,
            })
            .await?;
        println!("sandbox: {index_url}");
        println!("  Open this page for service links. Services must be running in the sandbox.");
        for preview in &previews {
            println!(
                "  {} (port {}): {}",
                preview.name, preview.port, preview.url
            );
        }
        if changed {
            let mut progress = crate::turn_display::TurnProgress::new();
            progress.set_status(Some("Preparing thread resources".into()));
            progress
                .wait(runtime.put_conversation_config(thread.as_ref(), config))
                .await?;
        }
        Ok(Some(session))
    }
}

fn dns_label(slug: &str) -> Result<String> {
    let normalized = crate::slugify(slug);
    let normalized = normalized.trim_matches('-');
    let label = normalized.chars().take(50).collect::<String>();
    let label = label.trim_end_matches('-');
    ensure!(
        !label.is_empty(),
        "preview names require a letter or digit in the agent and thread slugs"
    );
    Ok(label.to_owned())
}

fn thread_hostname(agent: &str, thread: &str, id: &str, domain: &str) -> Result<String> {
    let digest = format!("{:x}", Sha256::digest(id.as_bytes()));
    Ok(format!(
        "{}-{}.{}.{}",
        dns_label(thread)?,
        &digest[..8],
        dns_label(agent)?,
        domain.to_ascii_lowercase(),
    ))
}

async fn serve_gateway(
    listener: UnixListener,
    thread: Arc<dyn ConversationHandle>,
    ports: BTreeSet<u16>,
) {
    let result = crate::local_net::serve_connections(
        || async { Ok(listener.accept().await?.0) },
        move |client| {
            let thread = thread.clone();
            let ports = ports.clone();
            async move { handle_gateway(client, thread.as_ref(), &ports).await }
        },
        "preview",
        |error| tracing::debug!(%error, "preview connection failed"),
    )
    .await;
    if let Err(error) = result {
        eprintln!("preview listener failed: {error:#}");
    }
}

async fn handle_gateway(
    mut client: UnixStream,
    thread: &dyn ConversationHandle,
    ports: &BTreeSet<u16>,
) -> Result<()> {
    let port = timeout(Duration::from_secs(10), client.read_u16()).await??;
    ensure!(ports.contains(&port), "undeclared preview port");
    let mut upstream = match connect(thread, port).await {
        Ok(upstream) => upstream,
        Err(error) => {
            tracing::debug!(%error, "preview service unavailable");
            client.write_u8(0).await?;
            return Ok(());
        }
    };
    client.write_u8(1).await?;
    copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

async fn connect(
    thread: &dyn ConversationHandle,
    port: u16,
) -> Result<exoharness::BoxSandboxTcpStream> {
    let sandbox = crate::port_forward::published_sandbox(thread, port).await?;
    thread
        .connect_sandbox_tcp(sandbox, port)
        .await?
        .context("sandbox provider did not return a TCP connection")
}

pub(crate) async fn print(runtime: &Runtime, thread: &dyn ConversationHandle) -> Result<()> {
    let config = runtime.get_conversation_config(thread).await?;
    if config.browser_previews.is_empty() {
        bail!(
            "no previews have been assigned; configure previews in the environment and run this thread"
        );
    }
    if let Some(url) = &config.browser_preview_url {
        println!("sandbox: {url}");
    }
    crate::print_table(
        &["SERVICE", "PORT", "BROWSER URL"],
        config
            .browser_previews
            .into_iter()
            .map(|preview| vec![preview.name, preview.port.to_string(), preview.url])
            .collect(),
    )?;
    println!("Saved URLs: preview links require an open agent session and running services.");
    Ok(())
}

#[cfg(test)]
mod tests;
