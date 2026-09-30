use super::*;
use base64::Engine;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

const MAX_PROXY_PASSWORD_BYTES: usize = 8 * 1024;

/// Authenticates Basic proxy credentials and selects the sandbox session.
/// Return `None` to reject access (407) or an error if authorization is unavailable (503).
#[async_trait]
pub trait ProxyAuthorizer: Send + Sync {
    async fn authorize(
        &self,
        username: &str,
        password: &str,
        host: &str,
    ) -> Result<Option<ProxySession>>;
}

/// Authorized sandbox policy, TLS configuration, and credential placeholders.
#[derive(Clone)]
pub struct ProxySession {
    pub(super) state: Arc<State>,
    pub(super) tls: TlsAcceptor,
}

impl ProxySession {
    pub fn new(
        identity: EgressIdentity,
        policy: EgressPolicy,
        resolver: Option<Arc<dyn EgressCredentialResolver>>,
        request_authorizer: Option<Arc<dyn EgressRequestAuthorizer>>,
        tls: TlsAcceptor,
        placeholders: &HashMap<String, String>,
    ) -> Result<Self> {
        Ok(Self {
            state: Arc::new(State::new_with_placeholders(
                identity,
                policy,
                resolver,
                request_authorizer,
                Arc::new(PublicUpstreamResolver),
                Some(placeholders),
            )?),
            tls,
        })
    }
}

struct FixedAuthorizer {
    password: String,
    session: ProxySession,
}

#[async_trait]
impl ProxyAuthorizer for FixedAuthorizer {
    async fn authorize(
        &self,
        username: &str,
        password: &str,
        _host: &str,
    ) -> Result<Option<ProxySession>> {
        Ok((username == "exo" && password == self.password).then(|| self.session.clone()))
    }
}

/// Serves CONNECT requests, selecting a session through the authorizer before
/// accepting each tunnel. The session's policy is enforced before forwarding.
pub async fn serve_connect_proxy(
    listener: TcpListener,
    authorizer: Arc<dyn ProxyAuthorizer>,
    shutdown: CancellationToken,
) -> Result<()> {
    serve_explicit(listener, authorizer, shutdown, false, MAX_CONNECTIONS).await
}

pub struct ExplicitProxy {
    pub environment: HashMap<String, String>,
    pub ca_pem: String,
    pub ca_path: String,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl ExplicitProxy {
    pub async fn start(
        identity: EgressIdentity,
        policy: EgressPolicy,
        resolver: Option<Arc<dyn EgressCredentialResolver>>,
        listener: TcpListener,
        advertised_host: &str,
    ) -> Result<Self> {
        let state = State::new(identity, policy, resolver, Arc::new(PublicUpstreamResolver))?;
        Self::with_listener(state, listener, advertised_host).await
    }

    pub(super) async fn with_listener(
        state: State,
        listener: TcpListener,
        advertised_host: &str,
    ) -> Result<Self> {
        let password = uuid::Uuid::new_v4().simple().to_string();
        let proxy_url = format!(
            "http://exo:{password}@{advertised_host}:{}",
            listener.local_addr()?.port()
        );
        let (ca_pem, tls) = tls_configuration(state.hosts.iter().cloned().collect())?;
        let ca_path = format!("/tmp/exo-egress-{}.pem", uuid::Uuid::new_v4().simple());
        let mut environment: HashMap<_, _> = state
            .bindings
            .iter()
            .map(|binding| {
                (
                    binding.config.environment_variable.clone(),
                    binding.placeholder.clone(),
                )
            })
            .collect();
        for variable in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            environment.insert(variable.into(), proxy_url.clone());
        }
        for variable in ["NO_PROXY", "no_proxy"] {
            environment.insert(variable.into(), "localhost,127.0.0.1,::1".into());
        }
        environment.insert("NODE_USE_ENV_PROXY".into(), "1".into());
        for variable in [
            "SSL_CERT_FILE",
            "CODEX_CA_CERTIFICATE",
            "REQUESTS_CA_BUNDLE",
            "NODE_EXTRA_CA_CERTS",
            "CURL_CA_BUNDLE",
            "GIT_SSL_CAINFO",
        ] {
            environment.insert(variable.into(), ca_path.clone());
        }
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve_explicit(
            listener,
            Arc::new(FixedAuthorizer {
                password,
                session: ProxySession {
                    state: Arc::new(state),
                    tls,
                },
            }),
            cancel.clone(),
            true,
            MAX_CONNECTIONS,
        ));
        Ok(Self {
            environment,
            ca_pem,
            ca_path,
            cancel,
            task,
        })
    }

    pub fn command(&self, command: &crate::SandboxCommand) -> Result<crate::SandboxCommand> {
        ensure!(
            !self.is_closed(),
            "sandbox credential proxy is closed; acquire the sandbox again"
        );
        let mut command = command.clone();
        command.env.extend(self.environment.clone());
        Ok(command)
    }

    pub fn is_closed(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn close(&self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

impl Drop for ExplicitProxy {
    fn drop(&mut self) {
        self.close();
    }
}

pub(super) async fn serve_explicit(
    listener: TcpListener,
    authorizer: Arc<dyn ProxyAuthorizer>,
    cancel: CancellationToken,
    allow_http: bool,
    max_connections: usize,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    let permits = Arc::new(Semaphore::new(max_connections));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if !matches!(result, Ok(Ok(()))) {
                    tracing::debug!("explicit egress connection closed with an error");
                }
            }
            incoming = listener.accept() => {
                let (mut stream, _) = match incoming {
                    Ok(incoming) => incoming,
                    Err(error) => {
                        tracing::debug!(%error, "explicit egress accept failed; retrying");
                        if !super::transport::retry_after_error(&cancel).await {
                            break;
                        }
                        continue;
                    }
                };
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        result = tokio::time::timeout(Duration::from_secs(1), async {
                            stream.write_all(
                                b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                            ).await?;
                            stream.shutdown().await
                        }) => {
                            if !matches!(result, Ok(Ok(()))) {
                                tracing::debug!("could not send proxy overload response");
                            }
                        }
                    }
                    continue;
                };
                tasks.spawn(explicit_connection(stream, authorizer.clone(), cancel.clone(), Arc::new(permit), allow_http));
            }
        }
    }
    cancel.cancel();
    tasks.shutdown().await;
    Ok(())
}

fn error_response(status: StatusCode) -> Response<ProxyBody> {
    let body = Full::new(Bytes::from_static(
        b"sandbox proxy request denied or upstream unavailable\n",
    ))
    .map_err(|never| -> ProxyError { match never {} })
    .boxed();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    if status == StatusCode::PROXY_AUTHENTICATION_REQUIRED {
        response.headers_mut().insert(
            "proxy-authenticate",
            HeaderValue::from_static("Basic realm=\"Exo\""),
        );
    }
    response
}

async fn explicit_connection(
    stream: TcpStream,
    authorizer: Arc<dyn ProxyAuthorizer>,
    cancel: CancellationToken,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
    allow_http: bool,
) -> Result<()> {
    let connection_cancel = cancel.clone();
    let service = service_fn(move |request: Request<Incoming>| {
        let authorizer = authorizer.clone();
        let cancel = cancel.clone();
        let permit = permit.clone();
        async move {
            Ok::<_, Infallible>(
                proxy_request(request, authorizer, cancel, permit, allow_http).await,
            )
        }
    });
    tokio::select! {
        _ = connection_cancel.cancelled() => Ok(()),
        result = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new()).header_read_timeout(IO_TIMEOUT).max_buf_size(HTTP_BUFFER_SIZE)
            .serve_connection(TokioIo::new(stream), service).with_upgrades() => {
                result?;
                Ok(())
            }
    }
}

async fn proxy_request(
    mut request: Request<Incoming>,
    authorizer: Arc<dyn ProxyAuthorizer>,
    cancel: CancellationToken,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
    allow_http: bool,
) -> Response<ProxyBody> {
    let host = if request.method() == Method::CONNECT {
        match connect_target(&request) {
            Ok(host) => host,
            Err(_) => return error_response(StatusCode::BAD_REQUEST),
        }
    } else {
        if !allow_http {
            return error_response(StatusCode::METHOD_NOT_ALLOWED);
        }
        match request.uri().host().map(canonical_egress_host).transpose() {
            Ok(Some(host)) => host,
            _ => return error_response(StatusCode::BAD_REQUEST),
        }
    };
    let (username, password) = match basic_credentials(request.headers()) {
        Ok(credentials) => credentials,
        Err(_) => return error_response(StatusCode::PROXY_AUTHENTICATION_REQUIRED),
    };
    let session = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        authorizer.authorize(&username, &password, &host),
    )
    .await
    {
        Ok(Ok(Some(session))) => session,
        Ok(Ok(None)) => return error_response(StatusCode::PROXY_AUTHENTICATION_REQUIRED),
        Ok(Err(_)) | Err(_) => return error_response(StatusCode::SERVICE_UNAVAILABLE),
    };
    let result = if request.method() == Method::CONNECT {
        connect(&mut request, session.state, session.tls, cancel, permit).await
    } else {
        forward_http(request, session.state).await
    };
    match result {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!(%error, "explicit egress request failed");
            error_response(StatusCode::BAD_GATEWAY)
        }
    }
}

fn connect_target(request: &Request<Incoming>) -> Result<String> {
    let uri = request.uri();
    ensure!(
        uri.scheme().is_none() && uri.path().is_empty() && uri.query().is_none(),
        "CONNECT requires authority form"
    );
    let authority = uri.authority().context("CONNECT authority is required")?;
    ensure!(
        !authority.as_str().contains('@') && authority.port_u16() == Some(443),
        "CONNECT requires a host and port 443"
    );
    let host = canonical_egress_host(authority.host())?;
    ensure!(
        request.headers().get_all(HOST).iter().count() == 1,
        "exactly one Host header is required"
    );
    let header: hyper::http::uri::Authority = request
        .headers()
        .get(HOST)
        .context("Host header is required")?
        .to_str()?
        .parse()?;
    ensure!(
        !header.as_str().contains('@')
            && header.port_u16().is_none_or(|port| port == 443)
            && canonical_egress_host(header.host())? == host,
        "Host must match CONNECT authority",
    );
    ensure!(
        request.headers().get_all("content-length").iter().count() <= 1
            && request
                .headers()
                .get("content-length")
                .is_none_or(|length| length == "0")
            && !request.headers().contains_key("transfer-encoding"),
        "CONNECT body is not supported",
    );
    Ok(host)
}

fn basic_credentials(headers: &HeaderMap) -> Result<(String, String)> {
    ensure!(
        headers.get_all("proxy-authorization").iter().count() == 1,
        "exactly one Proxy-Authorization header is required"
    );
    let mut fields = headers
        .get("proxy-authorization")
        .context("Proxy-Authorization is required")?
        .to_str()?
        .split_whitespace();
    ensure!(
        fields
            .next()
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("Basic")),
        "Basic proxy authentication is required"
    );
    let encoded = fields.next().context("Basic credentials are required")?;
    ensure!(
        fields.next().is_none() && encoded.len() <= MAX_PROXY_PASSWORD_BYTES * 2,
        "invalid Basic credentials"
    );
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    let decoded = std::str::from_utf8(&decoded)?;
    let (username, password) = decoded
        .split_once(':')
        .context("Basic credentials require username and password")?;
    ensure!(
        !username.is_empty()
            && username.bytes().all(|byte| byte.is_ascii_graphic())
            && !password.is_empty()
            && password.len() <= MAX_PROXY_PASSWORD_BYTES
            && password.bytes().all(|byte| byte.is_ascii_graphic()),
        "invalid Basic credentials",
    );
    Ok((username.to_owned(), password.to_owned()))
}

async fn forward_http(
    mut request: Request<Incoming>,
    state: Arc<State>,
) -> Result<Response<ProxyBody>> {
    ensure!(
        request.uri().scheme_str() == Some("http"),
        "proxy requires an HTTP URL"
    );
    let authority = request
        .uri()
        .authority()
        .context("missing request authority")?;
    ensure!(
        request.headers().get_all(HOST).iter().count() == 1
            && request
                .headers()
                .get(HOST)
                .context("missing Host")?
                .to_str()?
                == authority.as_str(),
        "URL and Host must match"
    );
    *request.uri_mut() = request
        .uri()
        .path_and_query()
        .context("missing path")?
        .as_str()
        .parse()?;
    state.forward(request, None).await
}

async fn connect(
    request: &mut Request<Incoming>,
    state: Arc<State>,
    tls: TlsAcceptor,
    cancel: CancellationToken,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Response<ProxyBody>> {
    let host = state.connect_host(
        request
            .uri()
            .authority()
            .context("CONNECT requires an authority")?
            .as_str(),
    )?;
    let intercept = state.hosts.contains(&host);
    let upstream = if intercept {
        None
    } else {
        let addresses = state.upstream.resolve(&host, 443).await?.addresses;
        Some(
            tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addresses.as_slice()))
                .await??,
        )
    };
    let upgrade = hyper::upgrade::on(request);
    tokio::spawn(async move {
        let _permit = permit;
        let result: Result<()> = tokio::select! {
            _ = cancel.cancelled() => Ok(()),
            result = async {
                let mut stream = TokioIo::new(tokio::time::timeout(IO_TIMEOUT, upgrade).await??);
                if let Some(mut upstream) = upstream {
                    tokio::io::copy_bidirectional(&mut stream, &mut upstream).await?;
                } else {
                    https_connection(stream, tls, state, Some(&host)).await?;
                }
                Ok(())
            } => result,
        };
        if let Err(error) = result {
            tracing::debug!(%error, "explicit egress tunnel closed");
        }
    });
    Ok(Response::new(
        Full::new(Bytes::new())
            .map_err(|never| -> ProxyError { match never {} })
            .boxed(),
    ))
}
