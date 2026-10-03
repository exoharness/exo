use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use executor::BrowserPreview;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpStream, UnixListener, UnixStream},
    process::Command,
    task::JoinSet,
    time::{Instant, sleep, timeout},
};

use super::{read_request, reply, saved_listener};

#[derive(Serialize, Deserialize)]
struct ProxyAddress {
    socket: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Registration {
    pub gateway: PathBuf,
    pub portal: String,
    pub previews: Vec<BrowserPreview>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Response {
    Listening { port: u16 },
    Registered,
    Error { message: String },
}

pub(super) struct Client {
    stream: UnixStream,
    pub port: u16,
}

impl Client {
    pub async fn connect(directory: &Path) -> Result<Self> {
        let directory = directory.canonicalize()?;
        let stream = match existing_proxy(&directory).await? {
            Some(stream) => stream,
            None => {
                let log = directory.join("proxy.log");
                let mut child = Command::new(std::env::current_exe()?)
                    .arg("preview-proxy")
                    .arg(&directory)
                    .process_group(0)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(OpenOptions::new().create(true).append(true).open(&log)?)
                    .spawn()
                    .context("starting Exo's shared preview proxy")?;
                timeout(Duration::from_secs(10), async {
                    loop {
                        if let Some(stream) = existing_proxy(&directory).await? {
                            return Ok::<_, anyhow::Error>(stream);
                        }
                        if let Some(status) = child.try_wait()? {
                            ensure!(
                                status.success(),
                                "preview proxy exited: {status}; see {}",
                                log.display()
                            );
                        }
                        sleep(Duration::from_millis(25)).await;
                    }
                })
                .await
                .with_context(|| {
                    format!("starting preview proxy timed out; see {}", log.display())
                })??
            }
        };
        let mut client = Self { stream, port: 0 };
        match timeout(Duration::from_secs(10), receive(&mut client.stream)).await?? {
            Response::Listening { port } => client.port = port,
            _ => bail!("unexpected preview proxy greeting"),
        }
        Ok(client)
    }

    pub async fn register(&mut self, registration: Registration) -> Result<()> {
        send(&mut self.stream, &registration).await?;
        match timeout(Duration::from_secs(10), receive(&mut self.stream)).await?? {
            Response::Registered => Ok(()),
            Response::Error { message } => bail!("registering previews: {message}"),
            _ => bail!("unexpected preview registration response"),
        }
    }
}

async fn existing_proxy(directory: &Path) -> Result<Option<UnixStream>> {
    let bytes = match std::fs::read(directory.join("proxy.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let address: ProxyAddress = serde_json::from_slice(&bytes)?;
    match UnixStream::connect(&address.socket).await {
        Ok(stream) => Ok(Some(stream)),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error).context("connecting to the shared preview proxy"),
    }
}

#[derive(Clone)]
enum Route {
    Portal(Vec<BrowserPreview>),
    Service { gateway: PathBuf, port: u16 },
}

#[derive(Default)]
struct State {
    sessions: usize,
    routes: BTreeMap<String, Route>,
}

struct Lease {
    state: Arc<Mutex<State>>,
    hosts: Vec<String>,
}

impl Lease {
    fn new(state: Arc<Mutex<State>>) -> Self {
        state.lock().unwrap().sessions += 1;
        Self {
            state,
            hosts: Vec::new(),
        }
    }

    fn register(&mut self, registration: Registration) -> Result<()> {
        let mut routes = BTreeMap::from([(
            registration.portal,
            Route::Portal(registration.previews.clone()),
        )]);
        for preview in registration.previews {
            let host = preview
                .url
                .strip_prefix("http://")
                .context("preview URL must use HTTP")?;
            ensure!(
                routes
                    .insert(
                        host.to_owned(),
                        Route::Service {
                            gateway: registration.gateway.clone(),
                            port: preview.port,
                        }
                    )
                    .is_none(),
                "duplicate preview hostname"
            );
        }
        let mut state = self.state.lock().unwrap();
        ensure!(
            routes.keys().all(|host| !state.routes.contains_key(host)),
            "preview hostname is already registered"
        );
        self.hosts = routes.keys().cloned().collect();
        state.routes.extend(routes);
        Ok(())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        for host in &self.hosts {
            state.routes.remove(host);
        }
        state.sessions -= 1;
    }
}

pub(crate) async fn run(directory: &Path) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join("proxy.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let sockets = tempfile::tempdir_in("/tmp")?;
    let socket = sockets.path().join("proxy.sock");
    let control = UnixListener::bind(&socket)?;
    let listener = saved_listener(&directory.join("listener.json")).await?;
    let port = listener.local_addr()?.port();
    let mut address = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(address.as_file_mut(), &ProxyAddress { socket })?;
    address.persist(directory.join("proxy.json"))?;
    let state = Arc::new(Mutex::new(State::default()));
    let mut tasks = JoinSet::new();
    let mut idle = Instant::now();
    loop {
        tokio::select! {
            accepted = control.accept() => {
                let (stream, _) = accepted?;
                let lease = Lease::new(state.clone());
                tasks.spawn(register(stream, port, lease));
                idle = Instant::now();
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let state = state.clone();
                tasks.spawn(handle(stream, state));
            }
            completed = tasks.join_next(), if !tasks.is_empty() => {
                match completed {
                    Some(Ok(Err(error))) => tracing::debug!(%error, "preview connection failed"),
                    Some(Err(error)) => eprintln!("preview proxy task failed: {error}"),
                    _ => {},
                }
                idle = Instant::now();
            }
            () = sleep(Duration::from_secs(5)) => {
                if state.lock().unwrap().sessions == 0 && idle.elapsed() >= Duration::from_secs(5) {
                    return Ok(());
                }
            }
        }
    }
}

async fn register(mut stream: UnixStream, port: u16, mut lease: Lease) -> Result<()> {
    send(&mut stream, &Response::Listening { port }).await?;
    let registration = timeout(Duration::from_secs(10), receive(&mut stream)).await??;
    if let Err(error) = lease.register(registration) {
        send(
            &mut stream,
            &Response::Error {
                message: error.to_string(),
            },
        )
        .await?;
        return Ok(());
    }
    send(&mut stream, &Response::Registered).await?;
    let mut byte = [0];
    ensure!(
        stream.read(&mut byte).await? == 0,
        "unexpected preview control message"
    );
    Ok(())
}

async fn handle(mut client: TcpStream, state: Arc<Mutex<State>>) -> Result<()> {
    let (host, initial) = timeout(Duration::from_secs(10), read_request(&mut client)).await??;
    let route = state.lock().unwrap().routes.get(&host).cloned();
    match route {
        None => {
            reply(
                &mut client,
                "404 Not Found",
                "text/plain",
                "Unknown preview hostname",
            )
            .await
        }
        Some(Route::Portal(previews)) => {
            let links = previews
                .iter()
                .map(|preview| {
                    format!("<tr><td><a href=\"{}\">{}</a></td><td>{}</td><td><a href=\"{}\">{}</a></td></tr>", preview.url, preview.name, preview.port, preview.url, preview.url)
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
<table><thead><tr><th>Service</th><th>Sandbox port</th><th>Browser URL</th></tr></thead><tbody>{links}</tbody></table>
<p class="note">Start the services in your agent session, then open a link above. You can ask the agent to start them or help diagnose a connection problem.<br>Keep your Exo session open while using these links.</p>
</body></html>"#
            );
            reply(&mut client, "200 OK", "text/html", &body).await
        }
        Some(Route::Service { gateway, port }) => {
            let upstream = timeout(Duration::from_secs(10), async {
                let mut stream = UnixStream::connect(gateway).await?;
                stream.write_u16(port).await?;
                ensure!(stream.read_u8().await? == 1, "preview service unavailable");
                Ok::<_, anyhow::Error>(stream)
            })
            .await;
            let mut upstream = match upstream {
                Ok(Ok(upstream)) => upstream,
                error => {
                    tracing::debug!(?error, "preview service unavailable");
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
    }
}

async fn send<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= 65_536,
        "preview control message is too large"
    );
    stream.write_u32(bytes.len().try_into()?).await?;
    stream.write_all(&bytes).await?;
    Ok(())
}

async fn receive<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let length = stream.read_u32().await?;
    ensure!(length <= 65_536, "preview control message is too large");
    let mut bytes = vec![0; length.try_into()?];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests;
