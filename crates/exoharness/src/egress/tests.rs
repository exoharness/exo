use super::transport::dns_response;
use super::*;
use crate::CredentialInjectionLocation;
use anyhow::bail;
use hickory_proto::op::{Message, ResponseCode};
use hickory_proto::rr::RecordType;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

const GIT_REFS_PATH: &str = "/authorized-repo.git/info/refs?service=git-upload-pack";
const GIT_RECEIVE_REFS_PATH: &str = "/authorized-repo.git/info/refs?service=git-receive-pack";
const GIT_UPLOAD_PACK_PATH: &str = "/authorized-repo.git/git-upload-pack";
const GIT_RECEIVE_PACK_PATH: &str = "/authorized-repo.git/git-receive-pack";
const GIT_AUTHORIZATION: &str = "Basic eC1hY2Nlc3MtdG9rZW46Y2FuYXJ5LXYx";

impl EgressProxy {
    async fn shutdown(mut self) -> Result<()> {
        self.close();
        (&mut self.task).await.context("joining egress proxy")?;
        Ok(())
    }
}

#[derive(Clone)]
struct TestUpstream {
    address: SocketAddr,
    ca_pem: String,
}

#[async_trait]
impl UpstreamResolver for TestUpstream {
    async fn resolve(&self, _host: &str, _port: u16) -> Result<ResolvedUpstream> {
        Ok(ResolvedUpstream {
            addresses: vec![self.address],
            root_certificate: Some(reqwest::Certificate::from_pem(self.ca_pem.as_bytes())?),
        })
    }
}

struct TestResolver {
    value: RwLock<Option<String>>,
    uses: RwLock<Vec<(String, String, String)>>,
}

impl TestResolver {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            value: RwLock::new(Some("canary-v1".to_string())),
            uses: RwLock::new(Vec::new()),
        })
    }
}

#[async_trait]
impl EgressCredentialResolver for TestResolver {
    async fn resolve(
        &self,
        identity: &EgressIdentity,
        binding_name: &str,
        destination: &EgressDestination,
    ) -> Result<String> {
        ensure!(
            binding_name == "test-credential" && destination.port == 443,
            "wrong credential use"
        );
        self.uses.write().await.push((
            identity.sandbox_id.clone(),
            match &identity.scope {
                Some(crate::SandboxScope::Thread { thread_id, .. }) => thread_id.clone(),
                _ => bail!("expected thread identity"),
            },
            destination.host.clone(),
        ));
        self.value
            .read()
            .await
            .clone()
            .context("credential removed")
    }
}

struct GitResolver;

#[async_trait]
impl EgressCredentialResolver for GitResolver {
    async fn resolve(
        &self,
        _identity: &EgressIdentity,
        binding_name: &str,
        destination: &EgressDestination,
    ) -> Result<String> {
        let authorized_path = (destination.method == Method::GET
            && (destination.path == GIT_REFS_PATH || destination.path == GIT_RECEIVE_REFS_PATH))
            || (destination.method == Method::POST
                && (destination.path == GIT_UPLOAD_PACK_PATH
                    || destination.path == GIT_RECEIVE_PACK_PATH));
        ensure!(
            binding_name == "test-credential"
                && destination.host == "api.test"
                && destination.port == 443
                && authorized_path,
            "git request is not authorized"
        );
        Ok("eC1hY2Nlc3MtdG9rZW46Y2FuYXJ5LXYx".into())
    }

    async fn resolve_with_request(
        &self,
        identity: &EgressIdentity,
        binding_name: &str,
        destination: &EgressDestination,
        body: &[u8],
    ) -> Result<String> {
        if destination.method == Method::POST && destination.path == GIT_RECEIVE_PACK_PATH {
            ensure!(body == b"request", "unexpected Git push body");
        }
        self.resolve(identity, binding_name, destination).await
    }
}

struct Upstream {
    connections: Arc<AtomicUsize>,
    config: TestUpstream,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Upstream {
    async fn start() -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let (ca_pem, tls) = tls_configuration(vec!["api.test".into(), "public.test".into()])?;
        let config = TestUpstream {
            address: listener.local_addr()?,
            ca_pem,
        };
        let count = Arc::new(AtomicUsize::new(0));
        let connection_count = count.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    incoming = listener.accept() => {
                        let Ok((stream, _)) = incoming else { break; };
                        connection_count.fetch_add(1, Ordering::SeqCst);
                        let tls = tls.clone();
                        connections.spawn(async move {
                            let stream = tls.accept(stream).await?;
                            let service = service_fn(|request: Request<Incoming>| async move {
                                let authorization = request.headers().get("authorization")
                                    .or_else(|| request.headers().get("x-api-key"))
                                    .and_then(|h| h.to_str().ok());
                                let git_refs = request.method() == Method::GET
                                    && request.uri().path_and_query().is_some_and(|path| path.as_str() == GIT_REFS_PATH)
                                    && request.headers().get("git-protocol")
                                        .and_then(|h| h.to_str().ok()) == Some("version=2")
                                    && authorization == Some(GIT_AUTHORIZATION);
                                let git_upload_pack = request.method() == Method::POST
                                    && request.uri().path() == GIT_UPLOAD_PACK_PATH
                                    && request.headers().get("git-protocol")
                                        .and_then(|h| h.to_str().ok()) == Some("version=2")
                                    && authorization == Some(GIT_AUTHORIZATION);
                                let git_receive_pack = request.method() == Method::POST
                                    && request.uri().path() == GIT_RECEIVE_PACK_PATH
                                    && request.headers().get("git-protocol")
                                        .and_then(|h| h.to_str().ok()) == Some("version=2")
                                    && authorization == Some(GIT_AUTHORIZATION);
                                let git_receive_refs = request.method() == Method::GET
                                    && request.uri().path_and_query().is_some_and(|path| path.as_str() == GIT_RECEIVE_REFS_PATH)
                                    && request.headers().get("git-protocol")
                                        .and_then(|h| h.to_str().ok()) == Some("version=2")
                                    && authorization == Some(GIT_AUTHORIZATION);
                                let message = match authorization {
                                    Some("Bearer canary-v1") => "authenticated-v1",
                                    Some("canary-v1") => "raw-v1",
                                    Some("Token canary-v1; scope=read") => "token-v1",
                                    Some("Bearer canary-v2") => "authenticated-v2",
                                    None => "anonymous",
                                    _ => "bad-auth",
                                };
                                let body: BoxBody<Bytes, Infallible> = if git_refs {
                                    Full::new(Bytes::from_static(b"refs")).boxed()
                                } else if git_receive_refs {
                                    Full::new(Bytes::from_static(b"receive-refs")).boxed()
                                } else if git_upload_pack {
                                    use futures::StreamExt;
                                    let initial = futures::stream::once(async {
                                        Ok(Frame::data(Bytes::from_static(b"pack-1")))
                                    });
                                    let delayed = futures::stream::once(async {
                                        tokio::time::sleep(Duration::from_millis(25)).await;
                                        Ok(Frame::data(Bytes::from_static(b"pack-2")))
                                    });
                                    BodyExt::boxed(StreamBody::new(initial.chain(delayed)))
                                } else if git_receive_pack {
                                    Full::new(Bytes::from_static(b"receive-ok")).boxed()
                                } else if request.uri().path() == "/sse" {
                                    use futures::StreamExt;
                                    let initial = futures::stream::once(async { Ok(Frame::data(Bytes::from_static(b"first\n"))) });
                                    let delayed = futures::stream::once(async {
                                        tokio::time::sleep(IO_TIMEOUT + Duration::from_secs(1)).await;
                                        Ok(Frame::data(Bytes::from_static(b"last\n")))
                                    });
                                    BodyExt::boxed(StreamBody::new(initial.chain(delayed)))
                                } else {
                                    Full::new(Bytes::from_static(message.as_bytes())).boxed()
                                };
                                let mut response = Response::new(body);
                                if request.uri().path() == "/hop" {
                                    response.headers_mut().insert("connection", HeaderValue::from_static("x-hop, , invalid token,"));
                                    response.headers_mut().insert("x-hop", HeaderValue::from_static("remove-me"));
                                }
                                if request.uri().path() == "/redirect" {
                                    *response.status_mut() = StatusCode::FOUND;
                                    response.headers_mut().insert("location", HeaderValue::from_static("https://public.test/auth"));
                                }
                                Ok::<_, Infallible>(response)
                            });
                            hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await?;
                            Ok::<_, anyhow::Error>(())
                        });
                    }
                    Some(result) = connections.join_next(), if !connections.is_empty() => {
                        if !matches!(result, Ok(Ok(()))) { eprintln!("test upstream connection closed"); }
                    }
                }
            }
        });
        Ok(Self {
            connections: count,
            config,
            task,
        })
    }

    async fn proxy(
        &self,
        host_ip: Ipv4Addr,
        sandbox_id: &str,
        resolver: Arc<dyn EgressCredentialResolver>,
    ) -> Result<EgressProxy> {
        self.proxy_with_policy(host_ip, sandbox_id, resolver, policy())
            .await
    }

    async fn proxy_with_policy(
        &self,
        host_ip: Ipv4Addr,
        sandbox_id: &str,
        resolver: Arc<dyn EgressCredentialResolver>,
        policy: EgressPolicy,
    ) -> Result<EgressProxy> {
        let state = State::new(
            identity(sandbox_id),
            policy,
            Some(resolver),
            Arc::new(self.config.clone()),
        )?;
        let transport = Arc::new(LocalEgressTransport::bind(host_ip, &state.hosts).await?);
        EgressProxy::start_with_transport(transport, state, CancellationToken::new()).await
    }
}

struct ThreadResolver {
    credentials: HashMap<String, Vec<(EgressCredentialBinding, String)>>,
}

impl ThreadResolver {
    fn for_identity(
        &self,
        identity: &EgressIdentity,
    ) -> Result<&[(EgressCredentialBinding, String)]> {
        let Some(crate::SandboxScope::Thread { thread_id, .. }) = &identity.scope else {
            bail!("thread scope is required");
        };
        self.credentials
            .get(thread_id)
            .map(Vec::as_slice)
            .context("thread is not authorized")
    }
}

#[async_trait]
impl EgressCredentialResolver for ThreadResolver {
    async fn resolve(
        &self,
        identity: &EgressIdentity,
        binding_name: &str,
        _destination: &EgressDestination,
    ) -> Result<String> {
        self.for_identity(identity)?
            .iter()
            .find(|(binding, _)| binding.name == binding_name)
            .map(|(_, value)| value.clone())
            .context("binding is not selected for this thread")
    }
}

#[tokio::test]
async fn threads_select_different_bindings_and_resolve_the_same_name_independently() -> Result<()> {
    let binding = policy().credentials.remove(0);
    let extra = EgressCredentialBinding {
        name: "extra".into(),
        environment_variable: "EXTRA_API_KEY".into(),
        ..binding.clone()
    };
    let resolver = Arc::new(ThreadResolver {
        credentials: HashMap::from([
            (
                "thread-one".into(),
                vec![(binding.clone(), "canary-v1".into())],
            ),
            (
                "thread-two".into(),
                vec![(binding, "canary-v2".into()), (extra, "canary-v1".into())],
            ),
            ("thread-empty".into(), vec![]),
        ]),
    });
    let upstream = Upstream::start().await?;
    for (id, expected) in [("one", "authenticated-v1"), ("two", "authenticated-v2")] {
        let mut policy = policy();
        policy.credentials = resolver
            .for_identity(&identity(id))?
            .iter()
            .map(|(binding, _)| binding.clone())
            .collect();
        let proxy = upstream
            .proxy_with_policy(host_ip()?, id, resolver.clone(), policy)
            .await?;
        proxy.bind_source(host_ip()?).await?;
        assert_eq!(
            proxy.environment().contains_key("EXTRA_API_KEY"),
            id == "two"
        );
        let client = client(&proxy)?;
        assert_eq!(
            client
                .get("https://api.test/auth")
                .header(
                    "authorization",
                    format!("Bearer {}", proxy.environment()["TEST_API_KEY"])
                )
                .send()
                .await?
                .text()
                .await?,
            expected
        );
        if id == "two" {
            assert_eq!(
                client
                    .get("https://api.test/auth")
                    .header("x-api-key", &proxy.environment()["EXTRA_API_KEY"])
                    .send()
                    .await?
                    .text()
                    .await?,
                "raw-v1"
            );
        }
        proxy.shutdown().await?;
    }
    let mut empty_policy = policy();
    empty_policy.credentials.clear();
    let empty = upstream
        .proxy_with_policy(host_ip()?, "empty", resolver.clone(), empty_policy)
        .await?;
    assert!(empty.environment().is_empty());
    empty.shutdown().await?;
    let unauthorized = upstream.proxy(host_ip()?, "unknown", resolver).await?;
    unauthorized.bind_source(host_ip()?).await?;
    assert_eq!(
        client(&unauthorized)?
            .get("https://api.test/auth")
            .header(
                "authorization",
                format!("Bearer {}", unauthorized.environment()["TEST_API_KEY"])
            )
            .send()
            .await?
            .status(),
        StatusCode::BAD_GATEWAY
    );
    unauthorized.shutdown().await?;
    Ok(())
}

fn identity(sandbox_id: &str) -> EgressIdentity {
    EgressIdentity {
        sandbox_id: sandbox_id.into(),
        scope: Some(crate::SandboxScope::Thread {
            agent_id: "agent-1".into(),
            thread_id: format!("thread-{sandbox_id}"),
        }),
    }
}

fn policy() -> EgressPolicy {
    EgressPolicy {
        networking: SandboxNetworkPolicy::Limited {
            allowed_hosts: vec!["api.test".into(), "public.test".into()],
        },
        credentials: vec![EgressCredentialBinding {
            name: "test-credential".into(),
            environment_variable: "TEST_API_KEY".into(),
            networking: CredentialNetworkPolicy::Limited {
                allowed_hosts: vec!["api.test".into()],
            },
            injection_location: CredentialInjectionLocation { header: true },
        }],
    }
}

fn host_ip() -> Result<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 80))?;
    match socket.local_addr()?.ip() {
        IpAddr::V4(ip) => Ok(ip),
        _ => Err(anyhow!("test requires a local IPv4 address")),
    }
}

async fn bound_proxy(
    upstream: &Upstream,
    sandbox_id: &str,
    resolver: Arc<dyn EgressCredentialResolver>,
) -> Result<(EgressProxy, reqwest::Client)> {
    let proxy = upstream.proxy(host_ip()?, sandbox_id, resolver).await?;
    proxy.bind_source(host_ip()?).await?;
    let client = client(&proxy)?;
    Ok((proxy, client))
}

fn client(proxy: &EgressProxy) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .add_root_certificate(reqwest::Certificate::from_pem(proxy.ca_pem().as_bytes())?)
        .resolve("api.test", proxy.endpoints().https.into())
        .resolve("public.test", proxy.endpoints().https.into())
        .timeout(Duration::from_secs(10))
        .build()?)
}

#[tokio::test]
async fn proxy_requires_a_bound_source_and_closes_on_shutdown() -> Result<()> {
    let upstream = Upstream::start().await?;
    let resolver = TestResolver::new();
    let proxy = upstream.proxy(host_ip()?, "one", resolver.clone()).await?;
    let unbound_client = client(&proxy)?;
    assert!(
        unbound_client
            .get("https://api.test/auth")
            .send()
            .await
            .is_err()
    );
    proxy.bind_source(host_ip()?).await?;
    assert!(proxy.bind_source(host_ip()?).await.is_err());
    let proxy_client = client(&proxy)?;
    proxy.shutdown().await?;
    assert!(
        proxy_client
            .get("https://api.test/auth")
            .send()
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn proxy_substitutes_placeholders_in_header_formats() -> Result<()> {
    let upstream = Upstream::start().await?;
    let resolver = TestResolver::new();
    let (proxy, proxy_client) = bound_proxy(&upstream, "one", resolver.clone()).await?;
    let bearer = format!("Bearer {}", proxy.environment()["TEST_API_KEY"]);
    let response = proxy_client
        .get("https://api.test/auth")
        .header("authorization", &bearer)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await?, "authenticated-v1");
    assert_eq!(
        proxy_client
            .get("https://api.test/auth")
            .header("x-api-key", &proxy.environment()["TEST_API_KEY"])
            .send()
            .await?
            .text()
            .await?,
        "raw-v1"
    );
    for header in ["connection", "proxy-authorization"] {
        assert!(
            proxy_client
                .get("https://api.test/auth")
                .header(header, &bearer)
                .send()
                .await?
                .status()
                .is_server_error()
        );
    }
    assert_eq!(
        proxy_client
            .get("https://api.test/auth")
            .header("authorization", &proxy.environment()["TEST_API_KEY"])
            .send()
            .await?
            .text()
            .await?,
        "raw-v1"
    );
    assert_eq!(
        proxy_client
            .get("https://api.test/auth")
            .header(
                "authorization",
                format!("Token {}; scope=read", proxy.environment()["TEST_API_KEY"])
            )
            .send()
            .await?
            .text()
            .await?,
        "token-v1"
    );

    assert_eq!(
        proxy_client
            .get("https://public.test/auth")
            .send()
            .await?
            .text()
            .await?,
        "anonymous"
    );
    proxy.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn proxy_supports_git_smart_http() -> Result<()> {
    use futures::StreamExt;

    let upstream = Upstream::start().await?;
    let (proxy, proxy_client) = bound_proxy(&upstream, "git", Arc::new(GitResolver)).await?;
    let authorization = format!("Basic {}", proxy.environment()["TEST_API_KEY"]);

    let refs = proxy_client
        .get(format!("https://api.test{GIT_REFS_PATH}"))
        .header("authorization", &authorization)
        .header("git-protocol", "version=2")
        .send()
        .await?;
    assert_eq!(refs.status(), StatusCode::OK);
    assert_eq!(refs.text().await?, "refs");

    let mut pack = proxy_client
        .post(format!("https://api.test{GIT_UPLOAD_PACK_PATH}"))
        .header("authorization", &authorization)
        .header("git-protocol", "version=2")
        .body("request")
        .send()
        .await?
        .bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(2), pack.next())
        .await?
        .context("missing first pack chunk")?
        .context("reading first pack chunk")?;
    assert_eq!(first, Bytes::from_static(b"pack-1"));
    let second = tokio::time::timeout(Duration::from_secs(2), pack.next())
        .await?
        .context("missing second pack chunk")?
        .context("reading second pack chunk")?;
    assert_eq!(second, Bytes::from_static(b"pack-2"));
    assert!(pack.next().await.is_none());

    let receive_refs = proxy_client
        .get(format!("https://api.test{GIT_RECEIVE_REFS_PATH}"))
        .header("authorization", &authorization)
        .header("git-protocol", "version=2")
        .send()
        .await?;
    assert_eq!(receive_refs.status(), StatusCode::OK);
    assert_eq!(receive_refs.text().await?, "receive-refs");

    let receive = proxy_client
        .post(format!("https://api.test{GIT_RECEIVE_PACK_PATH}"))
        .header("authorization", &authorization)
        .header("git-protocol", "version=2")
        .body("request")
        .send()
        .await?;
    assert_eq!(receive.status(), StatusCode::OK);
    assert_eq!(receive.text().await?, "receive-ok");

    let rejected = proxy_client
        .post(format!("https://api.test{GIT_RECEIVE_PACK_PATH}"))
        .header("authorization", &authorization)
        .body("different update")
        .send()
        .await?;
    assert_eq!(rejected.status(), StatusCode::BAD_GATEWAY);

    assert_eq!(
        proxy_client
            .get("https://api.test/ungranted-repo.git/info/refs?service=git-upload-pack")
            .header("authorization", &authorization)
            .send()
            .await?
            .status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        proxy_client
            .post("https://api.test/ungranted-repo.git/git-receive-pack")
            .header("authorization", &authorization)
            .header("git-protocol", "version=2")
            .body("request")
            .send()
            .await?
            .status(),
        StatusCode::BAD_GATEWAY
    );

    proxy.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn proxy_resolves_current_credentials_for_each_request() -> Result<()> {
    let upstream = Upstream::start().await?;
    let resolver = TestResolver::new();
    let (proxy, proxy_client) = bound_proxy(&upstream, "one", resolver.clone()).await?;
    let bearer = format!("Bearer {}", proxy.environment()["TEST_API_KEY"]);
    *resolver.value.write().await = Some("canary-v2".into());
    assert_eq!(
        proxy_client
            .get("https://api.test/auth")
            .header("authorization", &bearer)
            .send()
            .await?
            .text()
            .await?,
        "authenticated-v2"
    );
    *resolver.value.write().await = None;
    assert!(
        proxy_client
            .get("https://api.test/auth")
            .header("authorization", &bearer)
            .send()
            .await?
            .status()
            .is_server_error()
    );
    assert!(
        resolver
            .uses
            .read()
            .await
            .iter()
            .all(|(sandbox, thread, host)| sandbox == "one"
                && thread == "thread-one"
                && host == "api.test")
    );
    proxy.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn proxy_rejects_cleartext_wrong_destinations_and_other_sandboxes() -> Result<()> {
    let upstream = Upstream::start().await?;
    let resolver = TestResolver::new();
    let (proxy, proxy_client) = bound_proxy(&upstream, "one", resolver.clone()).await?;
    let bearer = format!("Bearer {}", proxy.environment()["TEST_API_KEY"]);
    let cleartext = reqwest::Client::builder()
        .no_proxy()
        .resolve("api.test", proxy.endpoints().http.into())
        .timeout(Duration::from_secs(5))
        .build()?;
    assert!(
        cleartext
            .get("http://api.test/auth")
            .header("authorization", &bearer)
            .send()
            .await?
            .status()
            .is_server_error()
    );
    assert!(
        proxy_client
            .get("https://public.test/auth")
            .header("authorization", &bearer)
            .send()
            .await?
            .status()
            .is_server_error()
    );
    assert!(
        proxy_client
            .get("https://api.test/auth")
            .header(HOST, "public.test")
            .header("authorization", &bearer)
            .send()
            .await?
            .status()
            .is_server_error()
    );
    assert!(resolver.uses.read().await.is_empty());
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 0);
    let redirect = proxy_client
        .get("https://api.test/redirect")
        .header("authorization", &bearer)
        .send()
        .await?;
    assert_eq!(redirect.status(), StatusCode::FOUND);
    let other = upstream.proxy(host_ip()?, "two", resolver.clone()).await?;
    other.bind_source(host_ip()?).await?;
    assert!(
        client(&other)?
            .get("https://api.test/auth")
            .header("authorization", &bearer)
            .send()
            .await?
            .status()
            .is_server_error()
    );
    proxy.shutdown().await?;
    other.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn connect_stream_uses_supplied_placeholder_and_checks_both_hosts() -> Result<()> {
    let upstream = Upstream::start().await?;
    let resolver = TestResolver::new();
    let placeholder = "exo_egress_0123456789abcdef0123456789abcdef";
    let placeholders = HashMap::from([("TEST_API_KEY".into(), placeholder.into())]);
    let listener = Arc::new(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?);
    let (ca_pem, tls) = tls_configuration(vec!["api.test".into(), "public.test".into()])?;

    for (connect_host, request_host, expected_status) in [
        ("api.test", "api.test", Some(StatusCode::OK)),
        ("api.test", "public.test", Some(StatusCode::BAD_GATEWAY)),
        ("public.test", "api.test", None),
    ] {
        let state = Arc::new(State::new_with_placeholders(
            identity("connect"),
            policy(),
            Some(resolver.clone()),
            Arc::new(upstream.config.clone()),
            Some(&placeholders),
        )?);
        let accept = listener.clone();
        let tls = tls.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = accept.accept().await?;
            https_connection(stream, tls, state, Some(connect_host)).await
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes())?)
            .resolve("api.test", listener.local_addr()?)
            .build()?;
        let response = client
            .get("https://api.test/auth")
            .header(HOST, request_host)
            .header("connection", "close")
            .header("authorization", format!("Bearer {placeholder}"))
            .send()
            .await;
        if let Some(status) = expected_status {
            let response = response?;
            assert_eq!(response.status(), status);
            if status == StatusCode::OK {
                assert_eq!(response.text().await?, "authenticated-v1");
            }
            drop(client);
            server.await??;
        } else {
            assert!(response.is_err());
            assert!(server.await?.is_err());
        }
    }
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.uses.read().await.len(), 1);
    let mut no_credentials = policy();
    no_credentials.credentials.clear();
    for authority in ["blocked.test:443", "api.test:80"] {
        let (stream, _) = tokio::io::duplex(1);
        assert!(
            serve_https_connect(
                stream,
                authority,
                tls.clone(),
                identity("connect"),
                no_credentials.clone(),
                None,
                &HashMap::new(),
            )
            .await
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn rejects_unsafe_policy_and_addresses() {
    for host in [
        "*",
        "*.example.com",
        "127.0.0.1",
        "::1",
        "example.com:443",
        "example.com/",
        "a..com",
        "-a.com",
        "example.com.",
    ] {
        assert!(canonical_egress_host(host).is_err(), "{host}");
    }
    for ip in [
        "127.0.0.1",
        "10.0.0.1",
        "169.254.169.254",
        "100.100.100.200",
        "192.168.5.15",
        "198.19.0.1",
        "224.0.0.1",
        "::1",
        "::ffff:8.8.8.8",
        "2001:4860:4860::8888",
    ] {
        assert!(!public_ipv4(ip.parse().unwrap()), "{ip}");
    }
    assert!(public_ipv4("8.8.8.8".parse().unwrap()));
    assert!(public_ipv4("192.0.1.1".parse().unwrap()));
    let mut config = policy();
    config.credentials[0].networking = CredentialNetworkPolicy::Limited {
        allowed_hosts: vec!["blocked.test".into()],
    };
    let state = State::new(
        identity("one"),
        config,
        Some(TestResolver::new()),
        Arc::new(PublicUpstreamResolver),
    )
    .unwrap();
    assert!(!state.hosts.contains("blocked.test"));
    assert!(!state.bindings[0].permits_header("api.test"));
}

#[test]
fn credential_networking_is_independent_of_environment_networking() -> Result<()> {
    let mut config = policy();
    let state = State::new(
        identity("one"),
        config.clone(),
        Some(TestResolver::new()),
        Arc::new(PublicUpstreamResolver),
    )?;
    assert!(state.hosts.contains("public.test"));
    assert!(!state.hosts.contains("blocked.test"));
    assert!(state.bindings[0].permits_header("api.test"));
    assert!(!state.bindings[0].permits_header("public.test"));
    assert!(!state.bindings[0].permits_header("blocked.test"));
    config.credentials[0].injection_location.header = false;
    let state = State::new(
        identity("one"),
        config.clone(),
        Some(TestResolver::new()),
        Arc::new(PublicUpstreamResolver),
    )?;
    assert!(!state.bindings[0].permits_header("api.test"));
    assert!(
        serde_json::from_str::<CredentialInjectionLocation>(r#"{"header": true, "body": true}"#)
            .is_err()
    );
    let mut config = policy();
    config.networking = SandboxNetworkPolicy::Unrestricted;
    let state = State::new(
        identity("one"),
        config,
        Some(TestResolver::new()),
        Arc::new(PublicUpstreamResolver),
    )?;
    assert!(state.unrestricted);
    assert!(state.hosts.contains("api.test"));
    assert!(!state.hosts.contains("public.test"));
    assert!(!state.bindings[0].permits_header("public.test"));
    Ok(())
}

#[test]
fn empty_credential_allowlist_does_not_inherit_sandbox_hosts() -> Result<()> {
    let mut config = policy();
    config.credentials[0].networking = CredentialNetworkPolicy::Limited {
        allowed_hosts: vec![],
    };
    let state = State::new(
        identity("one"),
        config,
        Some(TestResolver::new()),
        Arc::new(PublicUpstreamResolver),
    )?;
    assert!(state.hosts.contains("api.test"));
    assert!(state.hosts.contains("public.test"));
    assert!(!state.bindings[0].permits_header("api.test"));
    assert!(!state.bindings[0].permits_header("public.test"));
    Ok(())
}

#[test]
fn dns_only_answers_exact_allowed_names() -> Result<()> {
    use hickory_proto::op::Query;
    use hickory_proto::rr::Name;
    let state = State::new(
        identity("one"),
        policy(),
        Some(TestResolver::new()),
        Arc::new(PublicUpstreamResolver),
    )?;
    for (host, kind, allowed) in [
        ("api.test", RecordType::A, true),
        ("api.test", RecordType::AAAA, true),
        ("api.test", RecordType::TXT, true),
        ("api.test", RecordType::HTTPS, true),
        ("leak.api.test", RecordType::A, false),
        ("blocked.test", RecordType::A, false),
    ] {
        let mut query = Message::new();
        query
            .set_id(123)
            .add_query(Query::query(Name::from_ascii(host)?, kind));
        let response = Message::from_vec(&dns_response(&state.hosts, &query.to_vec()?)?)?;
        assert_eq!(response.id(), 123);
        assert_eq!(
            response.response_code(),
            if allowed {
                ResponseCode::NoError
            } else {
                ResponseCode::NXDomain
            }
        );
        assert_eq!(
            response.answers().len(),
            usize::from(allowed && kind == RecordType::A)
        );
    }
    Ok(())
}

#[cfg(feature = "firecracker")]
async fn guest(
    handle: &Arc<dyn crate::ManagedSandboxHandle>,
    proxy: &EgressProxy,
    script: &str,
) -> Result<String> {
    let mut env = proxy.environment().clone();
    env.insert("EGRESS_CA".into(), proxy.ca_pem().into());
    let output = handle
        .exec(&crate::SandboxCommand {
            argv: vec!["python3".into(), "-c".into(), script.into()],
            env,
            display_argv: None,
            cwd: None,
            timeout: Some(Duration::from_secs(45)),
        })
        .await?;
    ensure!(
        output.ok,
        "guest check failed: {} {}",
        output.stdout,
        output.stderr
    );
    Ok(output.stdout)
}

#[cfg(feature = "firecracker")]
#[tokio::test]
#[ignore = "requires root, Linux/KVM, and the Exo Firecracker artifact bundle"]
async fn firecracker_transparent_egress_live() -> Result<()> {
    use crate::{
        FirecrackerConfig, FirecrackerSandboxBackend, ManagedSandboxBackend,
        SandboxLifecycleConfig, SandboxRequest, SandboxResourceShape, SandboxSpec,
    };
    ensure!(
        cfg!(target_os = "linux"),
        "this smoke test requires Linux/KVM"
    );
    let state_root = tempfile::Builder::new()
        .prefix("eg-")
        .tempdir_in("/var/lib/exo")?;
    let config = FirecrackerConfig {
        state_root: state_root.path().join("state"),
        allowed_egress_cidrs: vec!["0.0.0.0/0".parse()?],
        ..Default::default()
    };
    let backend = FirecrackerSandboxBackend::new(config).await?;
    let upstream = Upstream::start().await?;
    let resolver = TestResolver::new();
    let proxy = upstream
        .proxy(host_ip()?, "live-one", resolver.clone())
        .await?;
    let other = upstream
        .proxy(host_ip()?, "live-two", resolver.clone())
        .await?;
    let request = |id: &str| SandboxRequest {
        sandbox_id: id.into(),
        scope: None,
        provider_state: None,
        spec: SandboxSpec {
            image: crate::default_firecracker_image(),
            resources: SandboxResourceShape::new(1, 512).unwrap(),
            mounts: vec![],
            durable_file_systems: vec![],
            policy: policy(),
            default_workdir: "/home/exo/workspace".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
    };
    let one_request = request("egress-live-one");
    let two_request = request("egress-live-two");
    let result: Result<()> = async {
        let one = backend.acquire_request(crate::FirecrackerRequest {
            sandbox: one_request.clone(),
            egress_proxy: Some(proxy.endpoints()),
        }).await?;
        let one_source = backend.egress_source(one.as_ref()).await?.context("missing VM egress address")?;
        let two = backend.acquire_request(crate::FirecrackerRequest {
            sandbox: two_request.clone(),
            egress_proxy: Some(other.endpoints()),
        }).await?;
        let two_source = backend.egress_source(two.as_ref()).await?.context("missing VM egress address")?;
        proxy.bind_source(one_source).await?;
        other.bind_source(two_source).await?;
        let setup = r#"
import os, ssl, urllib.request, urllib.error, socket
assert os.environ['TEST_API_KEY'].startswith('exo_egress_')
assert 'canary-v1' not in str(os.environ)
assert 'canary-v2' not in str(os.environ)
ctx = ssl.create_default_context(cadata=os.environ['EGRESS_CA'])
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=ctx))
def get(host='api.test', token=True, header_host=None):
    headers = {'Authorization': 'Bearer ' + os.environ['TEST_API_KEY']} if token else {}
    if header_host: headers['Host'] = header_host
    try:
        with opener.open(urllib.request.Request('https://' + host + '/auth', headers=headers), timeout=5) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()
"#;
        let checks = format!("{setup}\n{}", r#"
assert get() == (200, 'authenticated-v1')
import subprocess, tempfile
with tempfile.NamedTemporaryFile(mode='w') as ca:
    ca.write(os.environ['EGRESS_CA'])
    ca.flush()
    result = subprocess.run(['curl', '--noproxy', '*', '--cacert', ca.name, '--max-time', '5', '-sS', '-H', 'Authorization: Bearer ' + os.environ['TEST_API_KEY'], 'https://api.test/auth'], check=True, capture_output=True, text=True)
    assert result.stdout == 'authenticated-v1'
assert get('public.test', False) == (200, 'anonymous')
assert get('public.test')[0] == 502
assert get(header_host='public.test')[0] == 502
try:
    socket.getaddrinfo('blocked.test', 443)
    raise AssertionError('blocked DNS resolved')
except socket.gaierror:
    pass
for address in [('1.1.1.1', 22), ('169.254.169.254', 22)]:
    try:
        socket.create_connection(address, timeout=2)
        raise AssertionError('direct egress succeeded')
    except OSError:
        pass
try:
    opener.open('https://1.1.1.1/', timeout=3)
    raise AssertionError('direct IP HTTPS succeeded')
except (urllib.error.URLError, OSError):
    pass
print('PASS transparent Python and curl HTTPS, anonymous host, wrong host/SNI, DNS, direct TCP/IP, no real credentials in guest')
"#);
        println!("{}", guest(&one, &proxy, &checks).await?);
        *resolver.value.write().await = Some("canary-v2".into());
        println!("{}", guest(&one, &proxy, &format!("{setup}\nassert get() == (200, 'authenticated-v2')\nprint('PASS rotation')")).await?);
        let stolen = proxy.environment()["TEST_API_KEY"].clone();
        let script = format!("{setup}\nos.environ['TEST_API_KEY'] = '{stolen}'\nassert get()[0] == 502\nprint('PASS cross-sandbox placeholder isolation')");
        println!("{}", guest(&two, &other, &script).await?);
        let attack = format!("{setup}\nimport socket\ntry:\n socket.create_connection(('{host}', {port}), timeout=2)\n raise AssertionError('another sandbox proxy is reachable')\nexcept OSError:\n pass\nprint('PASS cross-sandbox listener isolation')", host=proxy.endpoints().https.ip(), port=proxy.endpoints().https.port());
        println!("{}", guest(&two, &other, &attack).await?);
        *resolver.value.write().await = None;
        println!("{}", guest(&one, &proxy, &format!("{setup}\nassert get()[0] == 502\nprint('PASS removal')")).await?);
        proxy.cancel.cancel();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let stopped = format!("{setup}\ntry:\n get()\n raise AssertionError('stopped proxy still permits requests')\nexcept (urllib.error.URLError, OSError):\n pass\nprint('PASS proxy failure blocks egress')");
        println!("{}", guest(&one, &proxy, &stopped).await?);
        Ok(())
    }.await;
    let one_cleanup = backend.terminate(one_request).await;
    let two_cleanup = backend.terminate(two_request).await;
    proxy.shutdown().await?;
    other.shutdown().await?;
    one_cleanup?;
    two_cleanup?;
    result
}

#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "firecracker"))]
#[tokio::test]
#[ignore = "requires Firecracker artifacts; macOS also requires EXO_EGRESS_BRIDGE_BINARY in Lima"]
async fn managed_firecracker_egress_live() -> Result<()> {
    use crate::{
        ManagedSandboxBackend, SandboxCommand, SandboxLifecycleConfig, SandboxRequest,
        SandboxResourceShape, SandboxScope, SandboxSpec,
    };
    use tokio_util::compat::FuturesAsyncReadCompatExt;
    let mut config = crate::FirecrackerConfig::default();
    let state_root = format!(
        "/var/lib/exo/eg-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    config.state_root = std::path::PathBuf::from(&state_root);
    config.allowed_egress_cidrs = vec!["0.0.0.0/0".parse()?];
    let lima = crate::FirecrackerLimaConfig::default();
    #[cfg(target_os = "macos")]
    let lima = crate::FirecrackerLimaConfig {
        instance: std::env::var("EXO_FIRECRACKER_LIMA_INSTANCE").unwrap_or(lima.instance),
        bridge_binary: Some(
            std::env::var("EXO_EGRESS_BRIDGE_BINARY")
                .context("set EXO_EGRESS_BRIDGE_BINARY to the isolated Linux bridge binary")?
                .into(),
        ),
        ..lima
    };
    #[cfg(target_os = "macos")]
    let instance = lima.instance.clone();
    #[cfg(target_os = "linux")]
    let raw = {
        drop(lima);
        crate::FirecrackerSandboxBackend::new(config).await?
    };
    #[cfg(target_os = "macos")]
    let raw = crate::LimaFirecrackerSandboxBackend::new(config, lima).await?;
    let upstream = Upstream::start().await?;
    let resolver = TestResolver::new();
    let make_backend = || {
        raw.clone()
            .with_egress(Some(resolver.clone()), Arc::new(upstream.config.clone()))
    };
    let backend = make_backend();
    let request = SandboxRequest {
        sandbox_id: "managed-egress-live".into(),
        scope: Some(SandboxScope::Thread {
            agent_id: "agent-1".into(),
            thread_id: "managed-live".into(),
        }),
        provider_state: None,
        spec: SandboxSpec {
            image: crate::default_firecracker_image(),
            resources: SandboxResourceShape::new(1, 512).unwrap(),
            mounts: vec![],
            durable_file_systems: vec![],
            policy: policy(),
            default_workdir: "/home/exo/workspace".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
    };
    let command = |script: String| SandboxCommand {
        argv: vec!["python3".into(), "-c".into(), script],
        env: HashMap::from([("TEST_API_KEY".into(), "must-be-overridden".into())]),
        display_argv: None,
        cwd: None,
        timeout: Some(Duration::from_secs(30)),
    };
    let setup = r#"
import os, ssl, urllib.request, urllib.error, socket, pathlib, subprocess
assert os.environ['TEST_API_KEY'].startswith('exo_egress_')
assert 'canary-v1' not in str(os.environ)
assert 'canary-v2' not in str(os.environ)
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
def get(token=None):
    headers = {'Authorization': 'Bearer ' + (token or os.environ['TEST_API_KEY'])}
    try:
        with opener.open(urllib.request.Request('https://api.test/auth', headers=headers), timeout=5) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()
"#;
    let run = |extra: &str| command(format!("{setup}\n{extra}"));
    let result: Result<()> = async {
        let handle = backend.acquire(request.clone()).await?;
        let id = handle.id().to_owned();
        let check = handle.exec(&run(r#"
assert get() == (200, 'authenticated-v1')
result = subprocess.run(['curl', '--noproxy', '*', '--max-time', '5', '-sS', '-H', 'Authorization: Bearer ' + os.environ['TEST_API_KEY'], 'https://api.test/auth'], capture_output=True, text=True, check=True)
assert result.stdout == 'authenticated-v1'
pathlib.Path('egress-retained').write_text('keep this')
pathlib.Path('old-placeholder').write_text(os.environ['TEST_API_KEY'])
try:
    socket.getaddrinfo('blocked.test', 443)
    raise AssertionError('blocked DNS resolved')
except socket.gaierror:
    pass
try:
    socket.create_connection(('1.1.1.1', 22), timeout=2)
    raise AssertionError('direct egress succeeded')
except OSError:
    pass
print('PASS automatic placeholders, Python and curl trust, native transport, DNS and direct egress denial')
"#)).await?;
        ensure!(check.ok, "managed check failed: {}", check.stderr);
        println!("{}", check.stdout);
        *resolver.value.write().await = Some("canary-v2".into());
        let check = handle.exec(&run("assert get() == (200, 'authenticated-v2')\nprint('PASS rotation')")).await?;
        ensure!(check.ok, "rotation failed: {}", check.stderr);
        println!("{}", check.stdout);
        let reused = backend.acquire(request.clone()).await?;
        assert_eq!(reused.id(), id);
        let old_ca = handle.exec(&command("import os; print(os.environ['SSL_CERT_FILE'])".into())).await?.stdout;
        backend.shutdown_egress();
        let replacement = make_backend();
        let new_handle = replacement.acquire(request.clone()).await?;
        assert_eq!(new_handle.id(), id);
        let check = new_handle.exec(&run(r#"
assert pathlib.Path('egress-retained').read_text() == 'keep this'
assert get() == (200, 'authenticated-v2')
assert get(pathlib.Path('old-placeholder').read_text())[0] == 502
print('PASS reconnect retains VM files and rejects the previous placeholder')
"#)).await?;
        ensure!(check.ok, "reconnect failed: {}", check.stderr);
        println!("{}", check.stdout);
        let new_ca = new_handle.exec(&command("import os; print(os.environ['SSL_CERT_FILE'])".into())).await?.stdout;
        assert_ne!(new_ca, old_ca);
        let process = new_handle.start_process(&run("assert get() == (200, 'authenticated-v2')\nprint('PASS managed process environment')")).await?;
        let mut stdout = process.stdout.compat();
        let mut stderr = process.stderr.compat();
        let mut out = String::new();
        let mut err = String::new();
        let (exit, stdout_result, stderr_result) = tokio::join!(process.wait, tokio::io::AsyncReadExt::read_to_string(&mut stdout, &mut out), tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut err));
        let exit = exit?;
        stdout_result?;
        stderr_result?;
        ensure!(exit == 0, "managed process failed: {err}");
        println!("{out}");
        *resolver.value.write().await = None;
        let check = new_handle.exec(&run("assert get()[0] == 502\nprint('PASS revocation')")).await?;
        ensure!(check.ok, "revocation failed: {}", check.stderr);
        println!("{}", check.stdout);
        replacement.terminate(request.clone()).await?;
        Ok(())
    }.await;
    let cleanup = backend.terminate(request).await;
    cleanup?;
    #[cfg(target_os = "linux")]
    tokio::fs::remove_dir_all(&state_root).await?;
    #[cfg(target_os = "macos")]
    {
        let status = tokio::process::Command::new("limactl")
            .args([
                "shell",
                &instance,
                "--",
                "sudo",
                "-n",
                "rm",
                "-rf",
                &state_root,
            ])
            .status()
            .await?;
        ensure!(status.success(), "test state cleanup failed: {state_root}");
    }
    result
}

#[tokio::test]
async fn configured_listener_advertises_a_separate_address_and_binds_source() {
    use super::transport::{EgressTransport, LocalEgressTransport};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let advertised_address = Ipv4Addr::new(192, 0, 2, 2);
    let transport = LocalEgressTransport::with_config(
        crate::EgressListenConfig {
            bind_address: Ipv4Addr::LOCALHOST,
            advertised_address,
            http_port: 0,
            https_port: 0,
            dns_port: 0,
        },
        &["api.example.com".into()],
    )
    .await
    .unwrap();
    let endpoints = transport.endpoints();
    for endpoint in [endpoints.http, endpoints.https, endpoints.dns] {
        assert_eq!(*endpoint.ip(), advertised_address);
        assert_ne!(endpoint.port(), 0);
    }
    transport.bind_source(Ipv4Addr::LOCALHOST).await.unwrap();
    assert!(transport.bind_source(Ipv4Addr::LOCALHOST).await.is_err());
    let mut client = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, endpoints.https.port()))
        .await
        .unwrap();
    let mut stream = transport.accept(true).await.unwrap();
    client.write_all(b"request").await.unwrap();
    let mut buffer = [0; 7];
    stream.read_exact(&mut buffer).await.unwrap();
    assert_eq!(&buffer, b"request");
    transport.close();
    assert!(transport.accept(true).await.is_err());
}

#[tokio::test]
async fn closing_transport_releases_fixed_ports_with_retained_handles() -> Result<()> {
    let address = host_ip()?;
    let transport = Arc::new(LocalEgressTransport::bind(address, &HashSet::new()).await?);
    let endpoints = transport.endpoints();
    let accepting_transport = transport.clone();
    let accepting = tokio::spawn(async move { accepting_transport.accept(false).await });
    tokio::task::yield_now().await;
    transport.close();
    let replacement = LocalEgressTransport::with_config(
        crate::EgressListenConfig {
            bind_address: address,
            advertised_address: address,
            http_port: endpoints.http.port(),
            https_port: endpoints.https.port(),
            dns_port: endpoints.dns.port(),
        },
        &[],
    )
    .await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(1), accepting)
            .await??
            .is_err()
    );
    assert!(transport.is_closed());
    assert_eq!(replacement.endpoints(), endpoints);
    replacement.close();
    Ok(())
}

#[tokio::test]
async fn wildcard_listener_dns_replies_from_the_advertised_address_on_both_protocols() -> Result<()>
{
    use hickory_proto::{op::Query, rr::Name};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let address = host_ip()?;
    let transport = LocalEgressTransport::with_config(
        crate::EgressListenConfig {
            bind_address: Ipv4Addr::UNSPECIFIED,
            advertised_address: address,
            http_port: 0,
            https_port: 0,
            dns_port: 0,
        },
        &["api.test".into()],
    )
    .await?;
    transport.bind_source(address).await?;
    let mut query = Message::new();
    query.set_id(456).add_query(Query::query(
        Name::from_ascii("blocked.test")?,
        RecordType::A,
    ));
    let bytes = query.to_vec()?;
    let udp = tokio::net::UdpSocket::bind((address, 0)).await?;
    udp.connect(transport.endpoints().dns).await?;
    udp.send(&bytes).await?;
    let mut answer = [0; 4096];
    let size = tokio::time::timeout(Duration::from_secs(1), udp.recv(&mut answer)).await??;
    let response = Message::from_vec(&answer[..size])?;
    assert_eq!(response.id(), 456);
    assert_eq!(response.response_code(), ResponseCode::NXDomain);
    let mut tcp = tokio::net::TcpStream::connect(transport.endpoints().dns).await?;
    tcp.write_u16(bytes.len().try_into()?).await?;
    tcp.write_all(&bytes).await?;
    let size = tcp.read_u16().await?;
    let mut answer = vec![0; usize::from(size)];
    tcp.read_exact(&mut answer).await?;
    assert_eq!(
        Message::from_vec(&answer)?.response_code(),
        ResponseCode::NXDomain
    );
    transport.close();
    Ok(())
}

#[tokio::test]
async fn response_connection_list_does_not_fail_a_completed_write() -> Result<()> {
    let upstream = Upstream::start().await?;
    let (proxy, proxy_client) = bound_proxy(&upstream, "one", TestResolver::new()).await?;
    let response = proxy_client.post("https://api.test/hop").send().await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("x-hop"));
    assert_eq!(response.text().await?, "anonymous");
    proxy.shutdown().await
}

#[tokio::test]
async fn pooled_clients_reuse_connections_but_follow_changed_dns() -> Result<()> {
    struct ChangingUpstream(RwLock<TestUpstream>);
    #[async_trait]
    impl UpstreamResolver for ChangingUpstream {
        async fn resolve(&self, host: &str, port: u16) -> Result<ResolvedUpstream> {
            self.0.read().await.resolve(host, port).await
        }
    }
    let first = Upstream::start().await?;
    let second = Upstream::start().await?;
    let upstream = Arc::new(ChangingUpstream(RwLock::new(first.config.clone())));
    let state = State::new(
        identity("one"),
        policy(),
        Some(TestResolver::new()),
        upstream.clone(),
    )?;
    for _ in 0..2 {
        assert_eq!(
            state
                .client("api.test", 443)
                .await?
                .get("https://api.test/auth")
                .send()
                .await?
                .text()
                .await?,
            "anonymous"
        );
    }
    assert_eq!(first.connections.load(Ordering::SeqCst), 1);
    *upstream.0.write().await = second.config.clone();
    assert_eq!(
        state
            .client("api.test", 443)
            .await?
            .get("https://api.test/auth")
            .send()
            .await?
            .text()
            .await?,
        "anonymous"
    );
    assert_eq!(first.connections.load(Ordering::SeqCst), 1);
    assert_eq!(second.connections.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn quiet_response_stream_survives_past_the_request_io_timeout() -> Result<()> {
    let upstream = Upstream::start().await?;
    let (proxy, proxy_client) = bound_proxy(&upstream, "one", TestResolver::new()).await?;
    let mut response = proxy_client
        .get("https://api.test/sse")
        .timeout(IO_TIMEOUT * 2)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.chunk().await?.unwrap(), "first\n");
    // Advance only while the established stream is idle; resume real time before
    // waiting for OS socket readiness, which Tokio's clock does not simulate.
    tokio::time::pause();
    tokio::time::advance(IO_TIMEOUT + Duration::from_secs(1)).await;
    tokio::time::resume();
    assert_eq!(response.chunk().await?.unwrap(), "last\n");
    assert!(response.chunk().await?.is_none());
    proxy.shutdown().await
}

#[tokio::test]
async fn proxy_starts_without_a_sandbox_backend() -> Result<()> {
    let transport = Arc::new(
        LocalEgressTransport::bind(host_ip()?, &HashSet::from(["api.test".into()])).await?,
    );
    let proxy = EgressProxy::start(
        identity("hosted"),
        policy(),
        Some(TestResolver::new()),
        transport.clone(),
        CancellationToken::new(),
    )
    .await?;
    proxy.bind_source(host_ip()?).await?;
    assert_eq!(proxy.endpoints(), transport.endpoints());
    assert!(proxy.ca_pem().contains("BEGIN CERTIFICATE"));
    assert!(proxy.environment()["TEST_API_KEY"].starts_with(PLACEHOLDER_PREFIX));
    proxy.shutdown().await?;
    assert!(transport.is_closed());
    Ok(())
}

#[tokio::test]
async fn explicit_proxy_reuses_credential_substitution_and_isolates_sandboxes() -> Result<()> {
    use super::explicit::ExplicitProxy;
    let upstream = Upstream::start().await?;
    let resolver = TestResolver::new();
    let mut config = policy();
    config.networking = SandboxNetworkPolicy::Unrestricted;
    let start = |id| {
        State::new(
            identity(id),
            config.clone(),
            Some(resolver.clone()),
            Arc::new(upstream.config.clone()),
        )
    };
    let first = ExplicitProxy::with_listener(
        start("first")?,
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?,
        "127.0.0.1",
    )
    .await?;
    let second = ExplicitProxy::with_listener(
        start("second")?,
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?,
        "127.0.0.1",
    )
    .await?;
    let port = url::Url::parse(&first.environment["HTTPS_PROXY"])?
        .port()
        .unwrap();
    assert!(
        tokio::net::TcpStream::connect((host_ip()?, port))
            .await
            .is_err()
    );
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(&first.environment["HTTPS_PROXY"])?)
        .add_root_certificate(reqwest::Certificate::from_pem(first.ca_pem.as_bytes())?)
        .add_root_certificate(reqwest::Certificate::from_pem(
            upstream.config.ca_pem.as_bytes(),
        )?)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(45))
        .build()?;
    let placeholder = &first.environment["TEST_API_KEY"];
    assert!(placeholder.starts_with(PLACEHOLDER_PREFIX));
    for (value, expected) in [
        ("canary-v1", "authenticated-v1"),
        ("canary-v2", "authenticated-v2"),
    ] {
        *resolver.value.write().await = Some(value.into());
        assert_eq!(
            client
                .get("https://api.test/auth")
                .bearer_auth(placeholder)
                .send()
                .await?
                .text()
                .await?,
            expected
        );
    }
    assert_eq!(
        client
            .get("https://api.test/auth")
            .bearer_auth(&second.environment["TEST_API_KEY"])
            .send()
            .await?
            .status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        client
            .get("https://public.test/auth")
            .send()
            .await?
            .text()
            .await?,
        "anonymous"
    );
    let mut stream = client.get("https://api.test/sse").send().await?;
    assert_eq!(
        stream
            .chunk()
            .await?
            .context("missing first stream event")?
            .as_ref(),
        b"first\n"
    );
    tokio::time::pause();
    tokio::time::advance(IO_TIMEOUT + Duration::from_secs(1)).await;
    tokio::time::resume();
    assert_eq!(
        stream
            .chunk()
            .await?
            .context("missing last stream event")?
            .as_ref(),
        b"last\n"
    );
    *resolver.value.write().await = Some("canary-v1".into());
    assert_eq!(
        client
            .get(format!("https://api.test{GIT_REFS_PATH}"))
            .basic_auth("x-access-token", Some(placeholder))
            .header("git-protocol", "version=2")
            .send()
            .await?
            .text()
            .await?,
        "refs"
    );
    assert_eq!(
        client
            .get("https://api.test/auth")
            .basic_auth("x-access-token", Some(&second.environment["TEST_API_KEY"]))
            .send()
            .await?
            .status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        client
            .get("http://api.test/auth")
            .basic_auth("x-access-token", Some(placeholder))
            .send()
            .await?
            .status(),
        StatusCode::BAD_GATEWAY
    );
    *resolver.value.write().await = None;
    let rejected = client
        .get("https://api.test/auth")
        .bearer_auth(placeholder)
        .send()
        .await?;
    assert_eq!(rejected.status(), StatusCode::BAD_GATEWAY);
    assert!(!rejected.text().await?.contains("canary"));
    assert!(
        resolver
            .uses
            .read()
            .await
            .iter()
            .all(|(sandbox, _, host)| sandbox == "first" && host == "api.test")
    );
    let mut unauthenticated = url::Url::parse(&first.environment["HTTPS_PROXY"])?;
    unauthenticated.set_username("").unwrap();
    unauthenticated.set_password(None).unwrap();
    let denied = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(unauthenticated.as_str())?)
        .build()?;
    assert_eq!(
        denied.get("http://api.test/auth").send().await?.status(),
        StatusCode::PROXY_AUTHENTICATION_REQUIRED
    );
    assert!(denied.get("https://api.test/auth").send().await.is_err());
    first.close();
    assert!(client.get("https://api.test/auth").send().await.is_err());
    second.close();
    Ok(())
}

#[tokio::test]
async fn explicit_proxy_enforces_host_policy_without_credentials() -> Result<()> {
    let upstream = Upstream::start().await?;
    let resolver = TestResolver::new();
    let state = State::new(
        identity("limited"),
        policy(),
        Some(resolver.clone()),
        Arc::new(upstream.config.clone()),
    )?;
    let proxy = ExplicitProxy::with_listener(
        state,
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?,
        "127.0.0.1",
    )
    .await?;
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(&proxy.environment["HTTPS_PROXY"])?)
        .add_root_certificate(reqwest::Certificate::from_pem(proxy.ca_pem.as_bytes())?)
        .timeout(Duration::from_secs(5))
        .build()?;
    for url in ["https://blocked.test/auth", "https://api.test:8443/auth"] {
        assert!(client.get(url).send().await.is_err(), "{url}");
    }
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 0);
    assert_eq!(
        client
            .get("https://api.test/auth")
            .header(HOST, "public.test")
            .send()
            .await?
            .status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 0);
    assert_eq!(
        client
            .get("https://public.test/auth")
            .send()
            .await?
            .text()
            .await?,
        "anonymous"
    );
    assert!(resolver.uses.read().await.is_empty());
    proxy.close();
    Ok(())
}

struct TestProxyAuthorizer {
    sessions: HashMap<String, ProxySession>,
    ca_pem: String,
    calls: AtomicUsize,
}

impl TestProxyAuthorizer {
    fn new(upstream: &Upstream) -> Result<Arc<Self>> {
        let (ca_pem, tls) = tls_configuration(vec!["api.test".into(), "public.test".into()])?;
        let mut sessions = HashMap::new();
        for (name, value) in [("first", "canary-v1"), ("second", "canary-v2")] {
            let resolver = Arc::new(TestResolver {
                value: RwLock::new(Some(value.into())),
                uses: RwLock::new(Vec::new()),
            });
            let state = State::new(
                identity(name),
                policy(),
                Some(resolver),
                Arc::new(upstream.config.clone()),
            )?;
            sessions.insert(
                name.into(),
                ProxySession {
                    state: Arc::new(state),
                    tls: tls.clone(),
                },
            );
        }
        Ok(Arc::new(Self {
            sessions,
            ca_pem,
            calls: AtomicUsize::new(0),
        }))
    }
}

#[async_trait]
impl ProxyAuthorizer for TestProxyAuthorizer {
    async fn authorize(
        &self,
        username: &str,
        password: &str,
        _host: &str,
    ) -> Result<Option<ProxySession>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if password == "unavailable" {
            bail!("authorization service unavailable");
        }
        if password == "slow" {
            return std::future::pending().await;
        }
        Ok((username == "sandbox")
            .then(|| self.sessions.get(password).cloned())
            .flatten())
    }
}

async fn send_proxy_request(
    address: SocketAddr,
    request: &str,
) -> Result<(tokio::net::TcpStream, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(address).await?;
    stream.write_all(request.as_bytes()).await?;
    let response = tokio::time::timeout(IO_TIMEOUT, async {
        let mut response = Vec::new();
        loop {
            response.push(stream.read_u8().await?);
            if response.ends_with(b"\r\n\r\n") {
                return Ok::<_, anyhow::Error>(String::from_utf8(response)?);
            }
        }
    })
    .await??;
    Ok((stream, response))
}

fn proxy_connect_request(password: &str) -> String {
    format!(
        "CONNECT api.test:443 HTTP/1.1\r\nHost: api.test:443\r\nProxy-Authorization: Basic {}\r\n\r\n",
        base64::engine::general_purpose::STANDARD.encode(format!("sandbox:{password}")),
    )
}

#[tokio::test]
async fn hosted_proxy_selects_and_isolates_sessions_on_one_listener() -> Result<()> {
    let upstream = Upstream::start().await?;
    let authorizer = TestProxyAuthorizer::new(&upstream)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let server = tokio::spawn(serve_connect_proxy(
        listener,
        authorizer.clone(),
        cancel.clone(),
    ));
    let valid = proxy_connect_request("first");
    for request in [
        valid.replacen("CONNECT api.test:443", "CONNECT api.test:8443", 1),
        valid.replace("Host: api.test:443", "Host: public.test:443"),
        valid.replace(
            "Host: api.test:443",
            "Host: api.test:443\r\nHost: api.test:443",
        ),
        valid.replacen("CONNECT api.test:443", "CONNECT user@api.test:443", 1),
        valid.replacen("CONNECT api.test:443", "CONNECT https://api.test:443/", 1),
        valid.replace(
            "Host: api.test:443",
            "Host: api.test:443\r\nContent-Length: 1",
        ),
        valid.replace(
            "Host: api.test:443",
            "Host: api.test:443\r\nTransfer-Encoding: chunked",
        ),
    ] {
        let (_, response) = send_proxy_request(address, &request).await?;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    }
    for request in [
        "CONNECT api.test:443 HTTP/1.1\r\nHost: api.test:443\r\n\r\n",
        "CONNECT api.test:443 HTTP/1.1\r\nHost: api.test:443\r\nProxy-Authorization: Basic bad\r\n\r\n",
    ] {
        let (_, response) = send_proxy_request(address, request).await?;
        assert!(response.starts_with("HTTP/1.1 407"), "{response}");
    }
    let (_, response) = send_proxy_request(
        address,
        "GET http://api.test/ HTTP/1.1\r\nHost: api.test\r\n\r\n",
    )
    .await?;
    assert!(response.starts_with("HTTP/1.1 405"));
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    for (password, status) in [("unknown", "407"), ("unavailable", "503")] {
        let (_, response) = send_proxy_request(address, &proxy_connect_request(password)).await?;
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status}")),
            "{response}"
        );
    }
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 0);
    for (password, expected) in [
        ("first", "authenticated-v1"),
        ("second", "authenticated-v2"),
    ] {
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!(
                "http://sandbox:{password}@{address}"
            ))?)
            .add_root_certificate(reqwest::Certificate::from_pem(
                authorizer.ca_pem.as_bytes(),
            )?)
            .timeout(Duration::from_secs(5))
            .build()?;
        let session = &authorizer.sessions[password];
        assert_eq!(
            client
                .get("https://api.test/auth")
                .bearer_auth(&session.state.bindings[0].placeholder)
                .send()
                .await?
                .text()
                .await?,
            expected,
        );
        let other = &authorizer.sessions[if password == "first" {
            "second"
        } else {
            "first"
        }];
        assert_eq!(
            client
                .get("https://api.test/auth")
                .bearer_auth(&other.state.bindings[0].placeholder)
                .send()
                .await?
                .status(),
            StatusCode::BAD_GATEWAY,
        );
        assert!(
            client
                .get("https://blocked.test/auth")
                .send()
                .await
                .is_err()
        );
    }
    cancel.cancel();
    server.await??;
    Ok(())
}

#[tokio::test]
async fn hosted_proxy_bounds_connections_and_cancels_pending_tls() -> Result<()> {
    use tokio::io::AsyncReadExt;
    let upstream = Upstream::start().await?;
    let authorizer = TestProxyAuthorizer::new(&upstream)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let server = tokio::spawn(explicit::serve_explicit(
        listener,
        authorizer,
        cancel.clone(),
        false,
        1,
    ));
    let (mut active, response) =
        send_proxy_request(address, &proxy_connect_request("first")).await?;
    assert!(response.starts_with("HTTP/1.1 200"));
    let (_, overloaded) = send_proxy_request(address, &proxy_connect_request("first"))
        .await
        .context("reading overload response")?;
    assert!(overloaded.starts_with("HTTP/1.1 503"));
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(1), server).await???;
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), active.read(&mut byte))
            .await?
            .context("checking tunnel closed")?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn hosted_proxy_times_out_authorization_before_accepting_a_tunnel() -> Result<()> {
    let upstream = Upstream::start().await?;
    let authorizer = TestProxyAuthorizer::new(&upstream)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let server = tokio::spawn(serve_connect_proxy(
        listener,
        authorizer.clone(),
        cancel.clone(),
    ));
    let client =
        tokio::spawn(
            async move { send_proxy_request(address, &proxy_connect_request("slow")).await },
        );
    tokio::time::timeout(Duration::from_secs(2), async {
        while authorizer.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    tokio::time::pause();
    tokio::time::advance(CONNECT_TIMEOUT + Duration::from_secs(1)).await;
    tokio::time::resume();
    let (_, response) = client.await??;
    assert!(response.starts_with("HTTP/1.1 503"));
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 0);
    cancel.cancel();
    server.await??;
    Ok(())
}
