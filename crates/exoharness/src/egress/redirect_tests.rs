use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct DownloadRequest {
    method: String,
    host: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

pub(super) async fn respond(request: Request<Incoming>) -> Response<BoxBody<Bytes, Infallible>> {
    let path = request.uri().path();
    if path.starts_with("/redirect/") || path == "/download/chain" {
        let target = request
            .uri()
            .query()
            .and_then(|query| {
                url::form_urlencoded::parse(query.as_bytes())
                    .find(|(name, _)| name == "target")
                    .map(|(_, value)| value.into_owned())
            })
            .unwrap_or_else(|| {
                "https://public.test/download/echo?signature=upstream-issued".into()
            });
        return Response::builder()
            .status(StatusCode::FOUND)
            .header("location", target)
            .body(Full::new(Bytes::new()).boxed())
            .unwrap();
    }
    if path == "/download/oversized" {
        return Response::builder()
            .header(
                "content-length",
                super::super::redirect::MAX_REDIRECT_BODY_BYTES + 1,
            )
            .body(BodyExt::boxed(StreamBody::new(futures::stream::pending())))
            .unwrap();
    }
    if matches!(path, "/download/chunked" | "/download/slow") {
        use futures::StreamExt;
        let body = if path == "/download/chunked" {
            let chunk = Bytes::from(vec![b'x'; 1024 * 1024]);
            let count = super::super::redirect::MAX_REDIRECT_BODY_BYTES / chunk.len() + 1;
            BodyExt::boxed(StreamBody::new(futures::stream::iter(
                (0..count).map(move |_| Ok(Frame::data(chunk.clone()))),
            )))
        } else {
            BodyExt::boxed(StreamBody::new(
                futures::stream::once(async { Ok(Frame::data(Bytes::from_static(b"first"))) })
                    .chain(futures::stream::pending()),
            ))
        };
        return Response::new(body);
    }
    let (parts, body) = request.into_parts();
    let observed = DownloadRequest {
        method: parts.method.to_string(),
        host: parts.headers[HOST].to_str().unwrap().into(),
        path: parts.uri.to_string(),
        headers: parts
            .headers
            .iter()
            .filter(|(name, _)| !matches!(name.as_str(), "host" | "accept" | "accept-encoding"))
            .map(|(name, value)| (name.to_string(), value.to_str().unwrap().into()))
            .collect(),
        body: body.collect().await.unwrap().to_bytes().to_vec(),
    };
    Response::new(Full::new(Bytes::from(serde_json::to_vec(&observed).unwrap())).boxed())
}

struct DownloadPolicy {
    calls: AtomicUsize,
    allow_any_target: bool,
}

struct RedirectUpstream(TestUpstream);

#[async_trait]
impl UpstreamResolver for RedirectUpstream {
    async fn resolve(&self, host: &str, port: u16) -> Result<ResolvedUpstream> {
        if matches!(host, "api.test" | "public.test") {
            self.0.resolve(host, port).await
        } else {
            PublicUpstreamResolver.resolve(host, port).await
        }
    }
}

#[async_trait]
impl EgressRedirectPolicy for DownloadPolicy {
    async fn allow_redirect(
        &self,
        identity: &EgressIdentity,
        source: &EgressDestination,
        status: StatusCode,
        target: &reqwest::Url,
    ) -> Result<bool, EgressError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(identity.sandbox_id == "redirect-test"
            && source.host == "api.test"
            && source.path.split('?').next() == Some("/redirect/authorized")
            && status == StatusCode::FOUND
            && (self.allow_any_target || target.host_str() == Some("public.test")))
    }
}

struct RedirectProxy {
    client: reqwest::Client,
    placeholder: String,
    policy: Arc<DownloadPolicy>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl RedirectProxy {
    async fn start(upstream: &Upstream, enabled: bool, allow_any_target: bool) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let (ca_pem, tls) = tls_configuration(vec!["api.test".into()])?;
        let placeholder = format!("{PLACEHOLDER_PREFIX}0123456789abcdef0123456789abcdef");
        let redirect_policy = Arc::new(DownloadPolicy {
            calls: AtomicUsize::new(0),
            allow_any_target,
        });
        let mut config = policy();
        config.networking = SandboxNetworkPolicy::Limited {
            allowed_hosts: vec!["api.test".into()],
        };
        let mut session = ProxySession::new_with_redirect_policy(
            identity("redirect-test"),
            config,
            Some(TestResolver::new()),
            tls,
            &HashMap::from([("TEST_API_KEY".into(), placeholder.clone())]),
            enabled.then(|| redirect_policy.clone() as Arc<dyn EgressRedirectPolicy>),
        )?;
        Arc::get_mut(&mut session.state).unwrap().upstream =
            Arc::new(RedirectUpstream(upstream.config.clone()));
        let authorizer = Arc::new(TestProxyAuthorizer {
            sessions: HashMap::from([("password".into(), session)]),
            ca_pem: ca_pem.clone(),
            calls: AtomicUsize::new(0),
        });
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve_connect_proxy(listener, authorizer, cancel.clone()));
        let client = reqwest::Client::builder()
            .proxy(
                reqwest::Proxy::https(format!("http://{address}"))?
                    .basic_auth("sandbox", "password"),
            )
            .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes())?)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            client,
            placeholder,
            policy: redirect_policy,
            cancel,
            task,
        })
    }

    async fn shutdown(self) -> Result<()> {
        self.cancel.cancel();
        self.task.await??;
        Ok(())
    }
}

#[tokio::test]
async fn approved_redirect_streams_the_exact_url_without_original_headers_or_body() -> Result<()> {
    let upstream = Upstream::start().await?;
    let proxy = RedirectProxy::start(&upstream, true, false).await?;
    let response = proxy
        .client
        .get("https://api.test/redirect/authorized")
        .bearer_auth(&proxy.placeholder)
        .header("cookie", "private-cookie")
        .header("x-api-key", &proxy.placeholder)
        .header("x-private", "private-header")
        .body("private-body")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let observed: DownloadRequest = serde_json::from_slice(&response.bytes().await?)?;
    assert_eq!(
        observed,
        DownloadRequest {
            method: "GET".into(),
            host: "public.test".into(),
            path: "/download/echo?signature=upstream-issued".into(),
            headers: HashMap::new(),
            body: Vec::new(),
        }
    );
    assert_eq!(proxy.policy.calls.load(Ordering::SeqCst), 1);
    assert!(
        proxy
            .client
            .get("https://public.test/download/echo")
            .send()
            .await
            .is_err()
    );
    proxy.shutdown().await
}

#[tokio::test]
async fn redirects_require_opt_in_and_original_resource_and_target_authorization() -> Result<()> {
    let upstream = Upstream::start().await?;
    for (enabled, path, method, target, calls) in [
        (false, "/redirect/authorized", Method::GET, None, 0),
        (true, "/redirect/other-repository", Method::GET, None, 1),
        (
            true,
            "/redirect/authorized",
            Method::GET,
            Some("https://api.test/download/echo"),
            1,
        ),
        (true, "/redirect/authorized", Method::POST, None, 0),
    ] {
        let proxy = RedirectProxy::start(&upstream, enabled, false).await?;
        let mut request = proxy
            .client
            .request(method, format!("https://api.test{path}"));
        if let Some(target) = target {
            request = request.query(&[("target", target)]);
        }
        let response = request.send().await?;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(proxy.policy.calls.load(Ordering::SeqCst), calls);
        proxy.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn redirect_safety_checks_apply_even_when_the_policy_approves_any_target() -> Result<()> {
    let upstream = Upstream::start().await?;
    let proxy = RedirectProxy::start(&upstream, true, true).await?;
    for target in [
        "http://public.test/download/echo",
        "https://public.test:8443/download/echo",
        "https://user:password@public.test/download/echo",
        "https://public.test/download/echo#fragment",
        "https://127.0.0.1/download/echo",
        "https://localhost/download/echo",
        "https://public.test/download/chain",
    ] {
        let response = proxy
            .client
            .get("https://api.test/redirect/authorized")
            .query(&[("target", target)])
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{target}");
    }
    proxy.shutdown().await
}

#[tokio::test]
async fn redirect_downloads_enforce_declared_and_streamed_size_limits() -> Result<()> {
    let upstream = Upstream::start().await?;
    let proxy = RedirectProxy::start(&upstream, true, false).await?;
    let response = proxy
        .client
        .get("https://api.test/redirect/authorized")
        .query(&[("target", "https://public.test/download/oversized")])
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let response = proxy
        .client
        .get("https://api.test/redirect/authorized")
        .query(&[("target", "https://public.test/download/chunked")])
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.bytes().await.is_err());
    proxy.shutdown().await
}

#[tokio::test]
async fn redirect_download_timeout_includes_the_streamed_body() -> Result<()> {
    let upstream = Upstream::start().await?;
    let proxy = RedirectProxy::start(&upstream, true, false).await?;
    let mut response = proxy
        .client
        .get("https://api.test/redirect/authorized")
        .query(&[("target", "https://public.test/download/slow")])
        .send()
        .await?;
    assert_eq!(
        response.chunk().await?.as_deref(),
        Some(b"first".as_slice())
    );
    tokio::time::pause();
    tokio::time::advance(IO_TIMEOUT + Duration::from_secs(1)).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(1), response.bytes())
            .await?
            .is_err()
    );
    tokio::time::resume();
    proxy.shutdown().await
}
