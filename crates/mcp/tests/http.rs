use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use exo_mcp::{McpCredentials, McpServerConfig, McpToolSet};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone)]
struct ServerState {
    requests: Arc<Mutex<Vec<String>>>,
    sse: bool,
    anonymous: bool,
    failure: Option<(&'static str, StatusCode, bool)>,
    expire_session: Arc<AtomicBool>,
    token: Arc<Mutex<String>>,
}

impl Default for ServerState {
    fn default() -> Self {
        Self {
            requests: Arc::default(),
            sse: false,
            anonymous: false,
            failure: None,
            expire_session: Arc::default(),
            token: Arc::new(Mutex::new("fixture-token".into())),
        }
    }
}

#[derive(Deserialize)]
struct RpcRequest {
    id: Option<Value>,
    #[serde(flatten)]
    method: Method,
}

#[derive(Deserialize)]
#[serde(tag = "method", content = "params")]
enum Method {
    #[serde(rename = "initialize")]
    Initialize {
        #[serde(rename = "protocolVersion")]
        protocol_version: String,
    },
    #[serde(rename = "notifications/initialized")]
    Initialized,
    #[serde(rename = "tools/list")]
    List { cursor: Option<String> },
    #[serde(rename = "tools/call")]
    Call { name: String, arguments: CallArgs },
}

#[derive(Deserialize)]
struct CallArgs {
    query: String,
}

async fn rpc(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<RpcRequest>,
) -> Response {
    if headers.contains_key("mcp-session-id") && state.expire_session.swap(false, Ordering::SeqCst)
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let method = match &request.method {
        Method::Initialize { .. } => "initialize",
        Method::Initialized => "notifications/initialized",
        Method::List { .. } => "tools/list",
        Method::Call { .. } => "tools/call",
    };
    if let Some((phase, status, challenge)) = state.failure
        && method == phase
    {
        let mut response = status.into_response();
        if challenge {
            response
                .headers_mut()
                .insert("www-authenticate", "Bearer".parse().unwrap());
        }
        return response;
    }
    let expected = format!("Bearer {}", state.token.lock().unwrap());
    if !state.anonymous
        && headers.get("authorization").and_then(|h| h.to_str().ok()) != Some(expected.as_str())
    {
        return (StatusCode::UNAUTHORIZED, [("www-authenticate", "Bearer")]).into_response();
    }
    let result = match request.method {
        Method::Initialize { protocol_version } => {
            state.requests.lock().unwrap().push("initialize".into());
            json!({"protocolVersion":protocol_version,"capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
        }
        other => {
            assert_eq!(headers.get("mcp-session-id").unwrap(), "fixture-session");
            assert!(headers.contains_key("mcp-protocol-version"));
            assert_eq!(
                state.requests.lock().unwrap().first().map(String::as_str),
                Some("initialize")
            );
            match other {
                Method::Initialized => {
                    state.requests.lock().unwrap().push("initialized".into());
                    return StatusCode::ACCEPTED.into_response();
                }
                Method::List { cursor } => {
                    assert!(
                        state
                            .requests
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|s| s == "initialized")
                    );
                    state
                        .requests
                        .lock()
                        .unwrap()
                        .push(format!("list:{cursor:?}"));
                    let name = if cursor.is_none() {
                        "search"
                    } else {
                        assert_eq!(cursor.as_deref(), Some("page2"));
                        "other"
                    };
                    json!({"tools":[{"name":name,"description":"Search fixture","inputSchema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]},"outputSchema":{"type":"object"},"annotations":{"readOnlyHint":true,"destructiveHint":false}}],"nextCursor":if cursor.is_none(){Some("page2")}else{None}})
                }
                Method::Call { name, arguments } => {
                    state
                        .requests
                        .lock()
                        .unwrap()
                        .push(format!("call:{name}:{}", arguments.query));
                    if arguments.query == "rpc-error" {
                        return Json(json!({"jsonrpc":"2.0","id":request.id,"error":{"code":-32602,"message":"invalid fixture query"}})).into_response();
                    }
                    json!({"content":[{"type":"text","text":arguments.query}],"structuredContent":{"tool":name},"isError":arguments.query == "tool-error"})
                }
                Method::Initialize { .. } => unreachable!(),
            }
        }
    };
    let body = json!({"jsonrpc":"2.0","id":request.id,"result":result});
    if state.sse {
        (
            [
                ("content-type", "text/event-stream"),
                ("mcp-session-id", "fixture-session"),
            ],
            format!("event: message\ndata: {body}\n\n"),
        )
            .into_response()
    } else {
        ([("mcp-session-id", "fixture-session")], Json(body)).into_response()
    }
}

struct Fixture {
    url: String,
    state: ServerState,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start(sse: bool) -> Self {
        Self::with_state(ServerState {
            sse,
            ..Default::default()
        })
        .await
    }

    async fn with_state(state: ServerState) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/mcp", post(rpc).delete(|| async { StatusCode::OK }))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, state, task }
    }

    fn config(&self, name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            url: self.url.clone(),
            allowed_tools: None,
            blocked_tools: vec![],
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn credentials(servers: &[McpServerConfig]) -> McpCredentials {
    McpCredentials::from_env(
        servers,
        &servers
            .iter()
            .map(|s| format!("{}=FIXTURE_TOKEN", s.name))
            .collect::<Vec<_>>(),
        |_| Some("fixture-token".into()),
    )
    .unwrap()
}

#[tokio::test]
async fn discovers_filters_and_calls_json_and_sse_servers() {
    let json = Fixture::start(false).await;
    let sse = Fixture::start(true).await;
    let mut first = json.config("first");
    first.allowed_tools = Some(vec!["search".into()]);
    let mut second = sse.config("second");
    second.blocked_tools = vec!["other".into()];
    let servers = [first, second];
    let tools = McpToolSet::connect(&servers, credentials(&servers))
        .await
        .unwrap();
    assert_eq!(
        tools
            .tools()
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        ["exo_mcp__first__search", "exo_mcp__second__search"]
    );
    for tool in tools.tools() {
        assert_eq!(tool.tool_name, "search");
        assert_eq!(
            tool.annotations.as_ref().unwrap().read_only_hint,
            Some(true)
        );
        assert_eq!(
            tool.annotations.as_ref().unwrap().destructive_hint,
            Some(false)
        );
        assert_eq!(tool.output_schema, Some(json!({"type":"object"})));
        let result = tools
            .call(
                &tool.name,
                serde_json::from_value(json!({"query":"hello"})).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(result.structured_content, Some(json!({"tool":"search"})));
        assert_eq!(result.is_error, Some(false));
    }
    assert!(
        tools
            .call("exo_mcp__first__other", Default::default())
            .await
            .is_err()
    );
    for fixture in [&json, &sse] {
        assert_eq!(
            *fixture.state.requests.lock().unwrap(),
            [
                "initialize",
                "initialized",
                "list:None",
                "list:Some(\"page2\")",
                "call:search:hello"
            ]
        );
    }
    tools.close().await.unwrap();
}

#[tokio::test]
async fn preserves_tool_errors_and_reports_protocol_and_auth_errors() {
    let fixture = Fixture::start(false).await;
    let servers = [fixture.config("fixture")];
    let error = McpToolSet::connect(&servers, McpCredentials::default())
        .await
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("no credential was selected"));
    let tools = McpToolSet::connect(&servers, credentials(&servers))
        .await
        .unwrap();
    let result = tools
        .call(
            "exo_mcp__fixture__search",
            serde_json::from_value(json!({"query":"tool-error"})).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let error = tools
        .call(
            "exo_mcp__fixture__search",
            serde_json::from_value(json!({"query":"rpc-error"})).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("invalid fixture query"));
    tools.close().await.unwrap();
}

#[tokio::test]
async fn rejects_unknown_tools_and_missing_session_credentials() {
    let fixture = Fixture::start(false).await;
    let mut config = fixture.config("fixture");
    config.allowed_tools = Some(vec!["missing".into()]);
    let servers = [config];
    assert!(
        McpCredentials::from_env(&servers, &["fixture=MISSING_TOKEN".into()], |_| None).is_err()
    );
    assert!(
        McpCredentials::from_env(&servers, &["unknown=FIXTURE_TOKEN".into()], |_| Some(
            "fixture-token".into()
        ))
        .is_err()
    );
    let result = McpToolSet::connect(&servers, credentials(&servers)).await;
    assert!(format!("{:#}", result.err().unwrap()).contains("no tool named missing"));
}

#[tokio::test]
async fn public_servers_connect_without_credentials() {
    let fixture = Fixture::with_state(ServerState {
        anonymous: true,
        ..Default::default()
    })
    .await;
    let tools = McpToolSet::connect(&[fixture.config("public")], McpCredentials::default())
        .await
        .unwrap();
    assert_eq!(tools.tools().len(), 2);
    tools.close().await.unwrap();
}

#[tokio::test]
async fn authentication_challenges_distinguish_missing_credentials_from_rejected_tokens() {
    for phase in ["initialize", "tools/list", "tools/call"] {
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            for challenge in [false, true] {
                let fixture = Fixture::with_state(ServerState {
                    anonymous: true,
                    failure: Some((phase, status, challenge)),
                    ..Default::default()
                })
                .await;
                let servers = [fixture.config("fixture")];
                for supplied in [false, true] {
                    let credentials = if supplied {
                        credentials(&servers)
                    } else {
                        McpCredentials::default()
                    };
                    let connected = McpToolSet::connect(&servers, credentials).await;
                    let error = if phase == "tools/call" {
                        let tools = connected.unwrap();
                        let error = tools
                            .call(
                                "exo_mcp__fixture__search",
                                serde_json::from_value(json!({"query":"hello"})).unwrap(),
                            )
                            .await
                            .unwrap_err();
                        tools.close().await.unwrap();
                        error
                    } else {
                        connected.err().unwrap()
                    };
                    let message = format!("{error:#}");
                    let auth = error.downcast_ref::<exo_mcp::McpAuthenticationError>();
                    if status == StatusCode::INTERNAL_SERVER_ERROR || !challenge {
                        assert!(auth.is_none(), "{phase}: {message}");
                        assert!(!message.contains("MCP authentication failed"), "{message}");
                    } else {
                        let auth = auth.unwrap_or_else(|| panic!("{phase}: {message}"));
                        assert_eq!(auth.server_name, "fixture");
                        assert_eq!(auth.credential_supplied, supplied);
                        assert_eq!(
                            message
                                .matches("MCP authentication failed for fixture")
                                .count(),
                            1,
                            "{phase}: {message}"
                        );
                        assert_eq!(
                            message.contains("no credential was selected"),
                            !supplied,
                            "{message}"
                        );
                        assert_eq!(
                            message.contains("rejected the supplied credential"),
                            supplied,
                            "{message}"
                        );
                    }
                    assert!(
                        message.matches("Client error:").count() <= 1,
                        "{phase}: {message}"
                    );
                    assert!(!message.contains("fixture-token"), "{message}");
                }
            }
        }
    }
}

#[tokio::test]
#[ignore = "calls the public DeepWiki MCP server"]
async fn deepwiki_live() {
    let config = McpServerConfig {
        name: "deepwiki".into(),
        url: "https://mcp.deepwiki.com/mcp".into(),
        allowed_tools: Some(vec!["read_wiki_structure".into()]),
        blocked_tools: vec![],
    };
    let tools = McpToolSet::connect(&[config], McpCredentials::default())
        .await
        .unwrap();
    let result = tools
        .call(
            "exo_mcp__deepwiki__read_wiki_structure",
            serde_json::from_value(json!({"repoName":"tokio-rs/tokio"})).unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert!(!result.content.is_empty());
    tools.close().await.unwrap();
}

#[derive(Default)]
struct RotatingCredentials {
    token: Mutex<Option<(String, String)>>,
    resolutions: AtomicUsize,
    refresh_token: Mutex<Option<String>>,
    refreshes: AtomicUsize,
}

#[async_trait::async_trait]
impl exo_mcp::McpCredentialProvider for RotatingCredentials {
    async fn resolve(
        &self,
        _server: &McpServerConfig,
    ) -> anyhow::Result<Option<exo_mcp::McpCredential>> {
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        let state = self.token.lock().unwrap();
        let (version, token) = state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("credential was removed"))?;
        Ok(Some(exo_mcp::McpCredential {
            version: version.clone(),
            token: token.clone(),
        }))
    }

    async fn refresh(
        &self,
        _server: &McpServerConfig,
        rejected: &exo_mcp::McpCredential,
    ) -> anyhow::Result<Option<exo_mcp::McpCredential>> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        let fresh = self.refresh_token.lock().unwrap().take();
        Ok(fresh.map(|token| {
            *self.token.lock().unwrap() = Some((rejected.version.clone(), token.clone()));
            exo_mcp::McpCredential {
                version: rejected.version.clone(),
                token,
            }
        }))
    }
}

#[tokio::test]
async fn running_connections_observe_rotation_and_revocation() {
    let fixture = Fixture::start(false).await;
    let credentials = Arc::new(RotatingCredentials {
        token: Mutex::new(Some(("1".into(), "fixture-token".into()))),
        ..Default::default()
    });
    let tools =
        McpToolSet::connect_with_provider(&[fixture.config("fixture")], credentials.clone())
            .await
            .unwrap();
    let args = || serde_json::from_value(json!({"query":"hello"})).unwrap();
    let before = credentials.resolutions.load(Ordering::SeqCst);
    tools
        .call("exo_mcp__fixture__search", args())
        .await
        .unwrap();
    assert_eq!(credentials.resolutions.load(Ordering::SeqCst), before + 1);
    *fixture.state.token.lock().unwrap() = "rotated-token".into();
    *credentials.token.lock().unwrap() = Some(("2".into(), "rotated-token".into()));
    tools
        .call("exo_mcp__fixture__search", args())
        .await
        .unwrap();
    assert_eq!(
        fixture
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| *r == "initialize")
            .count(),
        1
    );
    *credentials.token.lock().unwrap() = Some(("expired".into(), "expired-token".into()));
    *credentials.refresh_token.lock().unwrap() = Some("rotated-token".into());
    tools
        .call("exo_mcp__fixture__search", args())
        .await
        .unwrap();
    assert_eq!(credentials.refreshes.load(Ordering::SeqCst), 1);
    fixture.state.expire_session.store(true, Ordering::SeqCst);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tools.call("exo_mcp__fixture__search", args()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        fixture
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| *r == "initialize")
            .count(),
        2
    );
    *credentials.token.lock().unwrap() = None;
    let before = fixture.state.requests.lock().unwrap().len();
    let error = tools
        .call("exo_mcp__fixture__search", args())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("credential was removed"));
    assert_eq!(fixture.state.requests.lock().unwrap().len(), before);
    *credentials.token.lock().unwrap() = Some(("3".into(), "invalid-token".into()));
    let error = tools
        .call("exo_mcp__fixture__search", args())
        .await
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("MCP tool fixture.search failed"),
        "{message}"
    );
    assert!(
        message.contains("rejected the supplied credential"),
        "{message}"
    );
    assert!(!message.contains("invalid-token"), "{message}");
    *credentials.token.lock().unwrap() = Some(("3".into(), "rotated-token".into()));
    tools
        .call("exo_mcp__fixture__search", args())
        .await
        .unwrap();
    tools.close().await.unwrap();
    assert!(
        tools
            .call("exo_mcp__fixture__search", args())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn redirects_never_forward_credentials() {
    let destination = Fixture::start(false).await;
    let url = destination.url.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/mcp",
        post(move || async move { axum::response::Redirect::temporary(&url) }),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut server = destination.config("fixture");
    server.url = format!("http://{address}/mcp");
    let servers = [server];
    assert!(
        McpToolSet::connect(&servers, credentials(&servers))
            .await
            .is_err()
    );
    assert!(destination.state.requests.lock().unwrap().is_empty());
    task.abort();
}
