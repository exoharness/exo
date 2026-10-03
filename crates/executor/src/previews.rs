use std::{
    collections::BTreeMap,
    net::Ipv4Addr,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
pub use exo_managed_agents::http::protocol::PreviewEndpoint;
use exoharness::{AgentId, EnvironmentDefinition, ThreadHandle, ThreadId, ThreadRecord};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserPreview {
    pub port: u16,
    pub url: String,
    host: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewUrls {
    pub page: String,
    pub services: Vec<BrowserPreview>,
    host: String,
}

/// Derive every browser URL from the environment, thread identity and listener.
pub fn previews_for(
    environment: Option<&EnvironmentDefinition>,
    thread: &ThreadRecord,
    endpoint: &PreviewEndpoint,
) -> Result<Option<PreviewUrls>> {
    let Some(environment) = environment.filter(|env| !env.config.tcp_ports.is_empty()) else {
        return Ok(None);
    };
    let slug: String = thread
        .slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(50)
        .collect();
    let slug = slug.trim_matches('-');
    ensure!(
        !slug.is_empty(),
        "preview thread names require an ASCII letter or digit"
    );
    let digest = format!("{:x}", Sha256::digest(thread.id.to_string().as_bytes()));
    let host = format!(
        "{slug}-{}.{}:{}",
        &digest[..8],
        endpoint.domain,
        endpoint.port
    );
    Ok(Some(PreviewUrls {
        page: format!("http://{host}"),
        host: host.clone(),
        services: environment
            .config
            .tcp_ports
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|port| BrowserPreview {
                port,
                url: format!("http://{port}.{host}"),
                host: format!("{port}.{host}"),
            })
            .collect(),
    }))
}

impl PreviewUrls {
    pub(crate) fn instructions(&self) -> String {
        let mut text = format!(
            "Exo sandbox previews:\nSandbox services page: {}\n",
            self.page
        );
        for service in &self.services {
            text.push_str(&format!(
                "- guest TCP port {}: {}\n",
                service.port, service.url
            ));
        }
        text.push_str("Use these browser origins for links, API URLs and CORS; preserve paths, queries and fragments. Inside the sandbox use guest ports. Start services first; 502 means a service is unreachable. Previews live while the owning Exo process runs.");
        text
    }
}

#[derive(Clone)]
enum Destination {
    Page(PreviewUrls),
    Service {
        thread: Arc<dyn ThreadHandle>,
        port: u16,
    },
}

#[derive(Clone)]
struct Route {
    agent: AgentId,
    thread: ThreadId,
    destination: Destination,
}

pub(crate) struct PreviewProxy {
    pub endpoint: PreviewEndpoint,
    routes: Arc<Mutex<BTreeMap<String, Route>>>,
    task: JoinHandle<()>,
}

impl PreviewProxy {
    pub async fn start_server(root: &Path, domain: &str) -> Result<Self> {
        #[derive(Serialize, Deserialize)]
        struct SavedListener {
            port: u16,
        }
        let path = root.join("previews/listener.json");
        let saved = match std::fs::read(&path) {
            Ok(bytes) => Some(serde_json::from_slice::<SavedListener>(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let proxy = Self::start(saved.as_ref().map(|saved| saved.port), domain).await?;
        if saved.is_none() {
            std::fs::create_dir_all(path.parent().context("preview listener directory")?)?;
            std::fs::write(
                path,
                serde_json::to_vec(&SavedListener {
                    port: proxy.endpoint.port,
                })?,
            )?;
        }
        Ok(proxy)
    }

    pub async fn start(port: Option<u16>, domain: &str) -> Result<Self> {
        ensure!(
            domain.len() <= 187
                && domain.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                }),
            "preview domain must be a lowercase DNS name of at most 187 bytes"
        );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port.unwrap_or(0)))
            .await
            .context("binding preview port; another process may be using the saved port")?;
        let endpoint = PreviewEndpoint {
            domain: domain.into(),
            port: listener.local_addr()?.port(),
        };
        let routes = Arc::new(Mutex::new(BTreeMap::new()));
        let routing = routes.clone();
        let task = tokio::spawn(async move {
            let result = crate::local_net::serve_connections(
                || async { Ok(listener.accept().await?.0) },
                move |client| {
                    let routes = routing.clone();
                    async move { handle(client, routes).await }
                },
                "preview",
                |error| tracing::debug!(%error, "preview connection failed"),
            )
            .await;
            if let Err(error) = result {
                eprintln!("preview listener failed: {error}");
            }
        });
        Ok(Self {
            endpoint,
            routes,
            task,
        })
    }

    pub fn register(&self, agent: AgentId, thread: Arc<dyn ThreadHandle>, previews: &PreviewUrls) {
        let id = thread.record().id;
        let mut routes = self.routes.lock().expect("preview routes poisoned");
        routes.retain(|_, route| route.thread != id);
        routes.insert(
            previews.host.clone(),
            Route {
                agent,
                thread: id,
                destination: Destination::Page(previews.clone()),
            },
        );
        for service in &previews.services {
            routes.insert(
                service.host.clone(),
                Route {
                    agent,
                    thread: id,
                    destination: Destination::Service {
                        thread: thread.clone(),
                        port: service.port,
                    },
                },
            );
        }
    }

    pub fn remove(&self, thread: ThreadId) {
        self.routes
            .lock()
            .expect("preview routes poisoned")
            .retain(|_, route| route.thread != thread);
    }

    pub fn remove_agent(&self, agent: AgentId) {
        self.routes
            .lock()
            .expect("preview routes poisoned")
            .retain(|_, route| route.agent != agent);
    }

    pub fn stop(&self) {
        self.task.abort();
    }
}

impl Drop for PreviewProxy {
    fn drop(&mut self) {
        self.stop();
    }
}

pub async fn published_sandbox(
    thread: &dyn ThreadHandle,
    port: u16,
) -> Result<exoharness::SandboxId> {
    thread
        .list_sandboxes()
        .await?
        .into_iter()
        .find(|sandbox| sandbox.running && sandbox.tcp_ports.contains(&port))
        .map(|sandbox| sandbox.id)
        .context("no running thread sandbox publishes the requested TCP port")
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
            return Ok((std::str::from_utf8(host.value)?.to_ascii_lowercase(), bytes));
        }
    }
}

async fn reply(client: &mut TcpStream, status: &str, content_type: &str, body: &str) -> Result<()> {
    client.write_all(format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}", body.len(),
    ).as_bytes()).await?;
    client.shutdown().await?;
    Ok(())
}

async fn handle(mut client: TcpStream, routes: Arc<Mutex<BTreeMap<String, Route>>>) -> Result<()> {
    let (host, initial) = timeout(Duration::from_secs(10), read_request(&mut client)).await??;
    let route = routes
        .lock()
        .expect("preview routes poisoned")
        .get(&host)
        .cloned();
    match route.map(|route| route.destination) {
        None => {
            reply(
                &mut client,
                "404 Not Found",
                "text/plain",
                "Unknown preview hostname",
            )
            .await
        }
        Some(Destination::Page(previews)) => {
            let links = previews
                .services
                .iter()
                .map(|service| {
                    let url = &service.url;
                    format!(
                        "<tr><td>{}</td><td><a href=\"{url}\">{url}</a></td></tr>",
                        service.port
                    )
                })
                .collect::<String>();
            let body = format!(
                r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Sandbox services · Exo</title>
<style>
:root {{ color-scheme: light dark; font-family: system-ui, sans-serif; }}
body {{ max-width: 1000px; margin: 64px auto; padding: 0 24px; }}
h1 {{ margin-bottom: 8px; }}
.address {{ color: light-dark(#555, #aaa); overflow-wrap: anywhere; }}
table {{ width: 100%; margin: 32px 0; border-collapse: collapse; text-align: left; }}
th, td {{ padding: 16px 12px; border-bottom: 1px solid light-dark(#ddd, #444); }}
td:last-child {{ overflow-wrap: anywhere; }}
a {{ color: light-dark(#245dcc, #90b6ff); text-underline-offset: 3px; }}
.note {{ line-height: 1.6; color: light-dark(#555, #aaa); }}
@media (max-width: 600px) {{ body {{ margin-top: 32px; padding: 0 16px; }} th, td {{ padding: 12px 6px; }} }}
</style></head><body><h1>Sandbox services</h1><p class="address">{host}</p>
<table><thead><tr><th>Sandbox port</th><th>Browser URL</th></tr></thead><tbody>{links}</tbody></table>
<p class="note">Start the services in your sandbox, then open a link above. You can ask the agent to start them or diagnose a connection problem.<br>Keep the owning Exo process running while using these links.</p>
</body></html>"#
            );
            reply(&mut client, "200 OK", "text/html", &body).await
        }
        Some(Destination::Service { thread, port }) => {
            let upstream = timeout(Duration::from_secs(10), async {
                let sandbox = published_sandbox(thread.as_ref(), port).await?;
                thread
                    .connect_sandbox_tcp(sandbox, port)
                    .await?
                    .context("sandbox provider did not return a TCP connection")
            })
            .await
            .context("connecting to preview service timed out")
            .and_then(|result| result);
            let mut upstream = match upstream {
                Ok(upstream) => upstream,
                Err(error) => {
                    tracing::debug!(%error, "preview service unavailable");
                    return reply(
                        &mut client,
                        "502 Bad Gateway",
                        "text/plain",
                        "Preview unavailable. Start the sandbox and its services.",
                    )
                    .await;
                }
            };
            upstream.write_all(&initial).await?;
            copy_bidirectional(&mut client, &mut upstream).await?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
