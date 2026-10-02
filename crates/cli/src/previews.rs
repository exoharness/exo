use std::{collections::BTreeMap, fs::File, net::Ipv4Addr, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use executor::{AgentHandle, BrowserPreview, ConversationHandle, Runtime};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedListener {
    port: u16,
}

pub(crate) struct PreviewSession {
    task: JoinHandle<()>,
    _lock: File,
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
        root: &Path,
    ) -> Result<Option<Self>> {
        let mut config = runtime.get_conversation_config(thread.as_ref()).await?;
        let preview_config = config
            .environment
            .as_ref()
            .and_then(|environment| environment.previews.clone());
        let Some(preview_config) = preview_config else {
            if !config.browser_previews.is_empty() {
                config.browser_previews.clear();
                runtime
                    .put_conversation_config(thread.as_ref(), config)
                    .await?;
            }
            return Ok(None);
        };
        let directory = root
            .join("previews")
            .join(agent.record().id.to_string())
            .join(thread.record().id.to_string());
        std::fs::create_dir_all(&directory)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join("session.lock"))?;
        lock.try_lock()
            .context("this thread already has a local preview session")?;
        let listener = saved_listener(&directory.join("listener.json")).await?;
        let port = listener.local_addr()?.port();
        let hostname = thread_hostname(
            &agent.record().slug,
            &thread.record().slug,
            &thread.record().id.to_string(),
            &preview_config.domain,
        )?;
        ensure!(
            preview_config
                .services
                .keys()
                .all(|name| name.len() + hostname.len() + 1 <= 253),
            "preview hostname exceeds the DNS length limit"
        );
        let previews: Vec<_> = preview_config
            .services
            .into_iter()
            .map(|(name, guest_port)| BrowserPreview {
                url: format!("http://{name}.{hostname}:{port}"),
                name,
                port: guest_port,
            })
            .collect();
        config.browser_previews = previews.clone();
        runtime
            .put_conversation_config(thread.as_ref(), config)
            .await?;
        println!("previews: http://{hostname}:{port}");
        for preview in &previews {
            println!(
                "  {} (port {}): {}",
                preview.name, preview.port, preview.url
            );
        }
        let portal = format!("{hostname}:{port}");
        let routes = previews
            .iter()
            .map(|preview| {
                (
                    preview.url.trim_start_matches("http://").to_owned(),
                    preview.port,
                )
            })
            .collect();
        let task = tokio::spawn(serve(listener, thread, portal, previews, routes));
        Ok(Some(Self { task, _lock: lock }))
    }
}

async fn saved_listener(path: &Path) -> Result<TcpListener> {
    let saved = match std::fs::read(path) {
        Ok(bytes) => Some(serde_json::from_slice::<SavedListener>(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, saved.as_ref().map_or(0, |s| s.port)))
        .await
        .context("binding this thread's saved preview port; another process may be using it")?;
    if saved.is_none() {
        std::fs::write(
            path,
            serde_json::to_vec(&SavedListener {
                port: listener.local_addr()?.port(),
            })?,
        )?;
    }
    Ok(listener)
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

async fn serve(
    listener: TcpListener,
    thread: Arc<dyn ConversationHandle>,
    portal: String,
    previews: Vec<BrowserPreview>,
    routes: BTreeMap<String, u16>,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (client, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        eprintln!("preview listener failed: {error}");
                        return;
                    }
                };
                let thread = thread.clone();
                let portal = portal.clone();
                let previews = previews.clone();
                let routes = routes.clone();
                connections.spawn(async move {
                    handle(client, thread, &portal, &previews, &routes).await
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                match completed {
                    Some(Ok(Err(error))) => tracing::debug!(%error, "preview connection failed"),
                    Some(Err(error)) => eprintln!("preview task failed: {error}"),
                    _ => {}
                }
            }
        }
    }
}

async fn handle(
    mut client: TcpStream,
    thread: Arc<dyn ConversationHandle>,
    portal: &str,
    previews: &[BrowserPreview],
    routes: &BTreeMap<String, u16>,
) -> Result<()> {
    let (host, initial) = timeout(Duration::from_secs(10), read_request(&mut client)).await??;
    if host == portal {
        let links = previews
            .iter()
            .map(|preview| {
                format!(
                    "<li><a href=\"{}\">{} (port {})</a></li>",
                    preview.url, preview.name, preview.port
                )
            })
            .collect::<String>();
        let body = format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>Exo previews</title></head><body><h1>Thread previews</h1><ul>{links}</ul><p>Start the services in your agent session to open a preview.</p></body></html>"
        );
        return reply(&mut client, "200 OK", "text/html", &body).await;
    }
    let Some(port) = routes.get(&host) else {
        return reply(
            &mut client,
            "404 Not Found",
            "text/plain",
            "Unknown preview hostname",
        )
        .await;
    };
    let mut upstream = match connect(thread.as_ref(), *port).await {
        Ok(upstream) => upstream,
        Err(error) => {
            tracing::debug!(%error, "preview service unavailable");
            return reply(
                &mut client,
                "502 Bad Gateway",
                "text/plain",
                "Preview unavailable. Resume the thread and start its services.",
            )
            .await;
        }
    };
    upstream.write_all(&initial).await?;
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

async fn read_request(client: &mut TcpStream) -> Result<(String, Vec<u8>)> {
    let mut bytes = Vec::new();
    loop {
        ensure!(
            bytes.len() < 32_768,
            "preview request headers are too large"
        );
        let mut chunk = [0; 4096];
        let count = client.read(&mut chunk).await?;
        ensure!(
            count > 0,
            "preview connection closed before request headers"
        );
        bytes.extend_from_slice(&chunk[..count]);
        let mut headers = [httparse::EMPTY_HEADER; 100];
        let mut request = httparse::Request::new(&mut headers);
        if request.parse(&bytes)?.is_complete() {
            let mut hosts = request
                .headers
                .iter()
                .filter(|h| h.name.eq_ignore_ascii_case("host"));
            let host = hosts.next().context("preview request has no Host header")?;
            ensure!(
                hosts.next().is_none(),
                "preview request has multiple Host headers"
            );
            let host = std::str::from_utf8(host.value)?.to_ascii_lowercase();
            return Ok((host, bytes));
        }
    }
}

async fn reply(client: &mut TcpStream, status: &str, content_type: &str, body: &str) -> Result<()> {
    client
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
                body.len(),
            )
            .as_bytes(),
        )
        .await?;
    client.shutdown().await?;
    Ok(())
}

pub(crate) async fn print(runtime: &Runtime, thread: &dyn ConversationHandle) -> Result<()> {
    let config = runtime.get_conversation_config(thread).await?;
    if config.browser_previews.is_empty() {
        bail!(
            "no previews have been assigned; configure previews in the environment and run this thread"
        );
    }
    crate::print_table(
        &["SERVICE", "PORT", "BROWSER URL"],
        config
            .browser_previews
            .into_iter()
            .map(|preview| vec![preview.name, preview.port.to_string(), preview.url])
            .collect(),
    )
}

#[cfg(test)]
mod tests;
