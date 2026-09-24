use anyhow::{Context, Result};
use exo_managed_agents::vaults::{VaultMcpCredentials, VaultSelection};
use exo_mcp::{McpServerConfig, McpToolSet};
use exoharness::vault::{SecretTarget, VaultHandle, global_vault};
use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, PutSecretRequest, SandboxBackendRegistration,
    SandboxProvider, Secret, SecretBackendChoice,
};
use serde::Deserialize;
use serde_json::json;
use std::{collections::HashMap, process::Stdio, sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, header, method, path},
};

struct Fixture {
    server: MockServer,
    temp: TempDir,
    vault: Arc<dyn VaultHandle>,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let server = MockServer::start().await;
        let temp = TempDir::new()?;
        std::fs::write(temp.path().join("prices.json"), "{}")?;
        let state = BasicExoHarness::new(config(&temp)).await?;
        let vault = global_vault(&state).await?;
        let base = server.uri();
        Mock::given(path("/mcp/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                format!("Bearer resource_metadata=\"{base}/resource\""),
            ))
            .with_priority(10)
            .mount(&server)
            .await;
        Mock::given(path("/resource")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "resource": format!("{base}/mcp/"), "authorization_servers": [base], "scopes_supported": ["workspace:read"]
        }))).mount(&server).await;
        Mock::given(path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": base, "authorization_endpoint": format!("{base}/authorize"), "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"), "code_challenge_methods_supported": ["S256"],
                "response_types_supported": ["code"], "token_endpoint_auth_methods_supported": ["none"],
                "scopes_supported": ["workspace:read", "offline_access"]
            }))).mount(&server).await;
        Mock::given(method("POST")).and(path("/register")).respond_with(|request: &wiremock::Request| {
            #[derive(Deserialize)]
            struct Registration { redirect_uris: Vec<String> }
            let registration: Registration = request.body_json().unwrap();
            ResponseTemplate::new(201).set_body_json(json!({"client_id": "vault-client", "redirect_uris": registration.redirect_uris}))
        }).mount(&server).await;
        Mock::given(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .respond_with(token("initial-token", Some("initial-refresh")))
            .mount(&server)
            .await;
        for verb in ["GET", "DELETE"] {
            Mock::given(method(verb))
                .and(path("/mcp/"))
                .respond_with(ResponseTemplate::new(405))
                .mount(&server)
                .await;
        }
        for value in ["initial-token", "refreshed-token"] {
            Mock::given(method("POST")).and(path("/mcp/")).and(header("authorization", format!("Bearer {value}")))
                .respond_with(|request: &wiremock::Request| {
                    #[derive(Deserialize)]
                    struct Rpc { id: Option<u64>, method: String }
                    let rpc: Rpc = request.body_json().unwrap();
                    let result = match rpc.method.as_str() {
                        "initialize" => json!({"protocolVersion": "2025-11-25", "capabilities": {"tools": {}}, "serverInfo": {"name": "workspace", "version": "1"}}),
                        "notifications/initialized" => return ResponseTemplate::new(202),
                        "tools/list" => json!({"tools": [{"name": "search", "inputSchema": {"type": "object", "properties": {}}}]}),
                        "tools/call" => json!({"content": [{"type": "text", "text": "Workspace result"}], "isError": false}),
                        other => panic!("unexpected MCP request {other}"),
                    };
                    if rpc.method != "initialize" {
                        assert_eq!(request.headers.get("mcp-session-id").unwrap(), "vault-session");
                    }
                    ResponseTemplate::new(200).insert_header("mcp-session-id", "vault-session").set_body_json(json!({"jsonrpc": "2.0", "id": rpc.id, "result": result}))
                }).mount(&server).await;
        }
        Ok(Self {
            server,
            temp,
            vault,
        })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_exo"));
        command
            .env_clear()
            .env("EXO_CONFIG_DIR", self.temp.path().join("config"))
            .current_dir(self.temp.path())
            .arg(args[0])
            .arg("--root")
            .arg(self.temp.path().join("state"))
            .args(["--secret-backend", "file", "--master-key-path"])
            .arg(self.temp.path().join("master-key"))
            .arg("--pricing-path")
            .arg(self.temp.path().join("prices.json"))
            .args(&args[1..])
            .kill_on_drop(true);
        command
    }

    async fn cli(&self, args: &[&str]) -> Result<String> {
        let output =
            tokio::time::timeout(Duration::from_secs(15), self.command(args).output()).await??;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    }

    async fn login(&self, update: bool, denied: bool) -> Result<()> {
        let url = format!("{}/mcp/", self.server.uri());
        let args = if update {
            vec![
                "vault",
                "secret",
                "update",
                "global",
                "notion",
                "--no-browser",
            ]
        } else {
            vec![
                "vault",
                "secret",
                "create",
                "global",
                "notion",
                "--mcp-server-url",
                &url,
                "--no-browser",
            ]
        };
        let mut child = self
            .command(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut lines = BufReader::new(child.stdout.take().context("CLI stdout")?).lines();
        let auth = tokio::time::timeout(Duration::from_secs(15), async {
            while let Some(line) = lines.next_line().await? {
                if line.starts_with("http://") {
                    return Ok::<_, anyhow::Error>(line);
                }
            }
            anyhow::bail!("CLI did not start OAuth")
        })
        .await??;
        let query: HashMap<String, String> =
            url::Url::parse(&auth)?.query_pairs().into_owned().collect();
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["resource"], url);
        let mut callback = url::Url::parse(&query["redirect_uri"])?;
        callback
            .query_pairs_mut()
            .append_pair("state", &query["state"]);
        if denied {
            callback
                .query_pairs_mut()
                .append_pair("error", "access_denied");
        } else {
            callback.query_pairs_mut().append_pair("code", "test-code");
        }
        let response = reqwest::get(callback).await?;
        assert_eq!(response.status(), if denied { 400 } else { 200 });
        let body = response.text().await?;
        assert!(body.contains(if denied {
            "Exo login failed"
        } else {
            "Authorization complete"
        }));
        let output =
            tokio::time::timeout(Duration::from_secs(15), child.wait_with_output()).await??;
        assert_eq!(
            output.status.success(),
            !denied,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    async fn secret_id(&self) -> Result<exoharness::SecretId> {
        Ok(self
            .vault
            .list_secrets()
            .await?
            .into_iter()
            .find(|s| s.name == "notion")
            .context("saved grant")?
            .id)
    }

    async fn expire(&self, id: &exoharness::SecretId) -> Result<()> {
        let mut secret = self.vault.get_secret(id).await?.context("grant")?;
        let Secret::Oauth { expires_at, .. } = &mut secret else {
            anyhow::bail!("expected OAuth grant")
        };
        *expires_at = Some(0);
        self.vault.update_secret(id, secret).await?;
        Ok(())
    }

    async fn connect(&self, vault: Arc<dyn VaultHandle>) -> Result<(McpToolSet, VaultSelection)> {
        let server = McpServerConfig {
            name: "workspace-tools".into(),
            url: format!("{}/mcp/", self.server.uri()),
            allowed_tools: None,
            blocked_tools: vec![],
        };
        let selection = VaultSelection::from_vaults(
            std::slice::from_ref(&vault),
            std::slice::from_ref(&server),
        )
        .await?;
        let tools = McpToolSet::connect_with_provider(
            &[server],
            Arc::new(VaultMcpCredentials::new(vec![vault], selection.clone())),
        )
        .await?;
        Ok((tools, selection))
    }

    fn target(&self) -> Result<SecretTarget> {
        SecretTarget::mcp(&format!("{}/mcp/", self.server.uri()))
    }
}

fn config(temp: &TempDir) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: temp.path().join("state/exoharness"),
        secret_backend: SecretBackendChoice::File {
            path: Some(temp.path().join("master-key")),
        },
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_policy: None,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
    }
}

fn token(access: &str, refresh: Option<&str>) -> ResponseTemplate {
    #[derive(serde::Serialize)]
    struct Token<'a> {
        access_token: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        refresh_token: Option<&'a str>,
        token_type: &'a str,
        expires_in: u64,
        scope: &'a str,
    }
    ResponseTemplate::new(200).set_body_json(Token {
        access_token: access,
        refresh_token: refresh,
        token_type: "Bearer",
        expires_in: 3600,
        scope: "workspace:read",
    })
}

#[tokio::test]
async fn vault_cli_oauth_survives_restart_refreshes_live_mcp_and_revokes() -> Result<()> {
    let f = Fixture::new().await?;
    f.login(false, false).await?;
    let id = f.secret_id().await?;
    let metadata = f
        .cli(&["vault", "secret", "get", "global", "notion"])
        .await?;
    assert!(metadata.contains("oauth"));
    for text in [
        metadata,
        std::fs::read_to_string(f.temp.path().join("state/exoharness/vaults/vaults.json"))?,
    ] {
        assert!(!text.contains("initial-token") && !text.contains("initial-refresh"));
    }
    let reopened = BasicExoHarness::new(config(&f.temp)).await?;
    let vault = global_vault(&reopened).await?;
    let (tools, selection) = f.connect(vault).await?;
    assert_eq!(selection.bindings[0].secret.as_ref().unwrap().secret_id, id);
    let tool = tools.tools()[0].name.clone();
    assert_eq!(
        tools.call(&tool, Default::default()).await?.is_error,
        Some(false)
    );
    f.expire(&id).await?;
    Mock::given(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=initial-refresh"))
        .and(body_string_contains("client_id=vault-client"))
        .respond_with(token("refreshed-token", Some("rotated-refresh")))
        .expect(1)
        .mount(&f.server)
        .await;
    assert_eq!(
        tools.call(&tool, Default::default()).await?.is_error,
        Some(false)
    );
    let secret = f.vault.get_secret(&id).await?.unwrap();
    assert!(
        matches!(secret, Secret::Oauth { access_token, refresh_token: Some(refresh), .. } if access_token == "refreshed-token" && refresh == "rotated-refresh")
    );
    f.cli(&["vault", "secret", "delete", "global", "notion"])
        .await?;
    assert!(tools.call(&tool, Default::default()).await.is_err());
    tools.close().await?;
    Ok(())
}

#[tokio::test]
async fn vault_refresh_is_coordinated_across_reopened_stores_and_rejects_wrong_destination()
-> Result<()> {
    let f = Fixture::new().await?;
    f.login(false, false).await?;
    let id = f.secret_id().await?;
    f.expire(&id).await?;
    let reopened = BasicExoHarness::new(config(&f.temp)).await?;
    let other = global_vault(&reopened).await?;
    assert!(
        other
            .resolve_secret(&id, &SecretTarget::mcp("https://other.example/mcp/")?)
            .await
            .is_err()
    );
    let mock = Mock::given(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(token("refreshed-token", None).set_delay(Duration::from_millis(100)))
        .expect(1)
        .mount_as_scoped(&f.server)
        .await;
    let target = f.target()?;
    let (a, b) = tokio::join!(
        f.vault.resolve_secret(&id, &target),
        other.resolve_secret(&id, &target)
    );
    let (a, b) = (a?, b?);
    assert_eq!(a.secret, b.secret);
    assert_eq!(a.revision, b.revision);
    assert!(
        matches!(a.secret, Secret::Oauth { refresh_token: Some(refresh), .. } if refresh == "initial-refresh")
    );
    drop(mock);
    Ok(())
}

#[tokio::test]
async fn failed_authorization_preserves_existing_vault_entry() -> Result<()> {
    let f = Fixture::new().await?;
    let id = f
        .vault
        .put_secret(PutSecretRequest {
            name: "notion".into(),
            target: Some(f.target()?),
            secret: Secret::Key {
                value: "existing-key".into(),
            },
        })
        .await?;
    f.login(true, true).await?;
    assert_eq!(
        f.vault.get_secret(&id).await?,
        Some(Secret::Key {
            value: "existing-key".into()
        })
    );
    assert_eq!(f.vault.list_secrets().await?[0].revision, 1);
    Ok(())
}

#[tokio::test]
async fn refresh_cannot_restore_a_revoked_or_manually_rotated_secret() -> Result<()> {
    for revoke in [true, false] {
        let f = Fixture::new().await?;
        f.login(false, false).await?;
        let id = f.secret_id().await?;
        f.expire(&id).await?;
        let pending = Mock::given(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(
                token("refreshed-token", Some("rotated-refresh"))
                    .set_delay(Duration::from_millis(200)),
            )
            .expect(1)
            .mount_as_scoped(&f.server)
            .await;
        let target = f.target()?;
        let vault = f.vault.clone();
        let task = tokio::spawn(async move { vault.resolve_secret(&id, &target).await });
        tokio::time::timeout(Duration::from_secs(5), pending.wait_until_satisfied()).await?;
        if revoke {
            f.vault.delete_secret(&id).await?;
        } else {
            f.vault
                .update_secret(
                    &id,
                    Secret::Key {
                        value: "manual-rotation".into(),
                    },
                )
                .await?;
        }
        assert!(task.await?.is_err());
        assert_eq!(
            f.vault.get_secret(&id).await?,
            if revoke {
                None
            } else {
                Some(Secret::Key {
                    value: "manual-rotation".into(),
                })
            }
        );
    }
    Ok(())
}

#[tokio::test]
async fn refresh_errors_and_redirects_leave_the_grant_intact() -> Result<()> {
    for status in [400, 503, 307] {
        let f = Fixture::new().await?;
        f.login(false, false).await?;
        let id = f.secret_id().await?;
        f.expire(&id).await?;
        let original = f.vault.get_secret(&id).await?;
        let response = if status == 307 {
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/leaked", f.server.uri()))
        } else {
            ResponseTemplate::new(status).set_body_json(json!({"error": if status == 400 { "invalid_grant" } else { "temporarily_unavailable" }}))
        };
        Mock::given(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(response)
            .expect(1)
            .mount(&f.server)
            .await;
        Mock::given(path("/leaked"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&f.server)
            .await;
        assert!(f.vault.resolve_secret(&id, &f.target()?).await.is_err());
        assert_eq!(f.vault.get_secret(&id).await?, original);
    }
    Ok(())
}

#[actix_web::test]
async fn rejected_tokens_refresh_once_without_reinitializing_the_mcp_session() -> Result<()> {
    for unknown_expiry in [false, true] {
        let f = Fixture::new().await?;
        f.login(false, false).await?;
        let id = f.secret_id().await?;
        if unknown_expiry {
            let mut secret = f.vault.get_secret(&id).await?.context("grant")?;
            let Secret::Oauth { expires_at, .. } = &mut secret else {
                panic!("expected OAuth")
            };
            *expires_at = None;
            f.vault.update_secret(&id, secret).await?;
        }
        let mut server = None;
        let vault = if unknown_expiry {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let remote = exoharness::HttpExoHarness::new(
                format!("http://{}", listener.local_addr()?),
                None,
            )?;
            let state = Arc::new(BasicExoHarness::new(config(&f.temp)).await?);
            server = Some(actix_web::rt::spawn(
                exoharness::serve_exoharness_http_listener(listener, state),
            ));
            let vault = global_vault(&remote).await?;
            assert!(
                vault
                    .refresh_secret(&id, &SecretTarget::mcp("https://other.example/mcp/")?, 1)
                    .await
                    .is_err()
            );
            vault
        } else {
            f.vault.clone()
        };
        let (tools, _) = f.connect(vault).await?;
        Mock::given(method("POST"))
            .and(path("/mcp/"))
            .and(header("authorization", "Bearer initial-token"))
            .and(body_string_contains("tools/call"))
            .respond_with(
                ResponseTemplate::new(401)
                    .insert_header("www-authenticate", "Bearer error=\"invalid_token\""),
            )
            .with_priority(1)
            .expect(1)
            .mount(&f.server)
            .await;
        Mock::given(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=initial-refresh"))
            .respond_with(token("refreshed-token", Some("rotated-refresh")))
            .expect(1)
            .mount(&f.server)
            .await;
        let tool = &tools.tools()[0].name;
        assert_eq!(
            tools.call(tool, Default::default()).await?.is_error,
            Some(false)
        );
        assert_eq!(
            tools.call(tool, Default::default()).await?.is_error,
            Some(false)
        );
        let requests = f.server.received_requests().await.context("requests")?;
        let initializations = requests
            .iter()
            .filter(|r| {
                r.url.path() == "/mcp/"
                    && String::from_utf8_lossy(&r.body).contains("\"initialize\"")
            })
            .count();
        assert_eq!(initializations, 2);
        assert!(
            matches!(f.vault.get_secret(&id).await?, Some(Secret::Oauth { refresh_token: Some(refresh), .. }) if refresh == "rotated-refresh")
        );
        tools.close().await?;
        if let Some(server) = server {
            server.abort();
        }
    }
    Ok(())
}

#[tokio::test]
async fn authentication_failure_during_initialize_can_refresh() -> Result<()> {
    let f = Fixture::new().await?;
    f.login(false, false).await?;
    Mock::given(method("POST"))
        .and(path("/mcp/"))
        .and(header("authorization", "Bearer initial-token"))
        .respond_with(ResponseTemplate::new(401).insert_header("www-authenticate", "Bearer"))
        .with_priority(1)
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(token("refreshed-token", Some("rotated-refresh")))
        .expect(1)
        .mount(&f.server)
        .await;
    let (tools, _) = f.connect(f.vault.clone()).await?;
    assert_eq!(tools.tools().len(), 1);
    tools.close().await?;
    Ok(())
}

#[tokio::test]
async fn refresh_survives_cancellation_and_persists_before_reuse() -> Result<()> {
    for forced in [false, true] {
        let f = Fixture::new().await?;
        f.login(false, false).await?;
        let id = f.secret_id().await?;
        if !forced {
            f.expire(&id).await?;
        }
        let revision = f.vault.list_secrets().await?[0].revision;
        let pending = Mock::given(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(
                token("refreshed-token", Some("rotated-refresh"))
                    .set_delay(Duration::from_millis(200)),
            )
            .expect(1)
            .mount_as_scoped(&f.server)
            .await;
        let target = f.target()?;
        let vault = f.vault.clone();
        let task = tokio::spawn(async move {
            if forced {
                vault.refresh_secret(&id, &target, revision).await
            } else {
                vault.resolve_secret(&id, &target).await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), pending.wait_until_satisfied()).await?;
        task.abort();
        assert!(
            task.await
                .err()
                .context("cancelled refresh caller")?
                .is_cancelled()
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(f.vault.get_secret(&id).await?, Some(Secret::Oauth { refresh_token: Some(refresh), .. }) if refresh == "rotated-refresh") {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await??;
        let reopened = BasicExoHarness::new(config(&f.temp)).await?;
        let fresh = global_vault(&reopened)
            .await?
            .resolve_secret(&id, &f.target()?)
            .await?;
        assert_eq!(fresh.revision, revision + 1);
        assert!(
            matches!(fresh.secret, Secret::Oauth { access_token, .. } if access_token == "refreshed-token")
        );
    }
    Ok(())
}

#[tokio::test]
async fn concurrent_rejections_share_refresh_and_recheck_destination() -> Result<()> {
    let f = Fixture::new().await?;
    f.login(false, false).await?;
    let id = f.secret_id().await?;
    let target = f.target()?;
    let revision = f.vault.resolve_secret(&id, &target).await?.revision;
    let reopened = BasicExoHarness::new(config(&f.temp)).await?;
    let other = global_vault(&reopened).await?;
    assert!(
        other
            .refresh_secret(
                &id,
                &SecretTarget::mcp("https://other.example/mcp/")?,
                revision
            )
            .await
            .is_err()
    );
    Mock::given(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(
            token("refreshed-token", Some("rotated-refresh")).set_delay(Duration::from_millis(100)),
        )
        .expect(1)
        .mount(&f.server)
        .await;
    let (a, b) = tokio::join!(
        f.vault.refresh_secret(&id, &target, revision),
        other.refresh_secret(&id, &target, revision)
    );
    let (a, b) = (a?, b?);
    assert_eq!(a.revision, revision + 1);
    assert_eq!(a.revision, b.revision);
    assert_eq!(a.secret, b.secret);
    Ok(())
}

#[tokio::test]
async fn mcp_auth_retries_are_bounded_and_only_for_refreshable_401s() -> Result<()> {
    for (status, refreshable, expected_calls, expected_refreshes) in [
        (401, true, 2, 1),
        (401, false, 1, 0),
        (403, true, 1, 0),
        (500, true, 1, 0),
    ] {
        let f = Fixture::new().await?;
        f.login(false, false).await?;
        if !refreshable {
            f.vault
                .update_secret(
                    &f.secret_id().await?,
                    Secret::Key {
                        value: "initial-token".into(),
                    },
                )
                .await?;
        }
        let (tools, _) = f.connect(f.vault.clone()).await?;
        Mock::given(method("POST"))
            .and(path("/mcp/"))
            .and(body_string_contains("tools/call"))
            .respond_with(ResponseTemplate::new(status).insert_header("www-authenticate", "Bearer"))
            .with_priority(1)
            .expect(expected_calls)
            .mount(&f.server)
            .await;
        Mock::given(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(token("refreshed-token", Some("rotated-refresh")))
            .expect(expected_refreshes)
            .mount(&f.server)
            .await;
        assert!(
            tools
                .call(&tools.tools()[0].name, Default::default())
                .await
                .is_err()
        );
        tools.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn chat_vault_smoke_restart_second_vault_rotation_revocation_and_public_mcp() -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let f = Fixture::new().await?;
    f.login(false, false).await?;
    let output = f
        .command(&[
            "vault",
            "secret",
            "create",
            "global",
            "model",
            "--token-env",
            "MODEL_TEST_KEY",
        ])
        .env("MODEL_TEST_KEY", "unused-test-model-key")
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    f.cli(&[
        "model",
        "create",
        "gpt-5-mini",
        "--secret",
        "model",
        "--base-url",
        &f.server.uri(),
    ])
    .await?;
    let file = f.temp.path().join("vault-agent.md");
    std::fs::write(
        &file,
        format!(
            "---\nname: Vault smoke\nharness: basic\nconfig:\n  model: gpt-5-mini\nmcp_servers:\n  - type: url\n    name: workspace\n    url: {}/mcp/\n---\nAnswer workspace questions.\n",
            f.server.uri()
        ),
    )?;
    f.cli(&[
        "agent",
        "create",
        "vault-smoke",
        "--file",
        file.to_str().unwrap(),
    ])
    .await?;
    async fn chat(f: &Fixture, args: &[&str]) -> Result<std::process::Output> {
        let mut child = f
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("stdin")?
            .write_all(b"/quit\n")
            .await?;
        Ok(tokio::time::timeout(Duration::from_secs(15), child.wait_with_output()).await??)
    }
    fn started(output: std::process::Output) -> Result<String> {
        anyhow::ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout)?;
        anyhow::ensure!(stdout.contains("mcp: 1 tools"), "{stdout}");
        Ok(stdout
            .lines()
            .find_map(|line| line.strip_prefix("thread: "))
            .context("thread")?
            .split_once(" (")
            .context("thread slug")?
            .0
            .to_owned())
    }
    let first = started(chat(&f, &["agent", "run", "--agent", "vault-smoke"]).await?)?;
    started(
        chat(
            &f,
            &["agent", "run", "--agent", "vault-smoke", "--thread", &first],
        )
        .await?,
    )?;
    f.cli(&["vault", "create", "second"]).await?;
    let url = format!("{}/mcp/", f.server.uri());
    let added = f
        .command(&[
            "vault",
            "secret",
            "create",
            "second",
            "workspace",
            "--mcp-server-url",
            &url,
            "--token-env",
            "MCP_TOKEN",
        ])
        .env("MCP_TOKEN", "refreshed-token")
        .output()
        .await?;
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let second = started(
        chat(
            &f,
            &[
                "agent",
                "run",
                "--agent",
                "vault-smoke",
                "--vault",
                "second",
            ],
        )
        .await?,
    )?;
    let rotated = f
        .command(&[
            "vault",
            "secret",
            "update",
            "second",
            "workspace",
            "--token-env",
            "MCP_TOKEN",
        ])
        .env("MCP_TOKEN", "initial-token")
        .output()
        .await?;
    assert!(rotated.status.success());
    started(
        chat(
            &f,
            &[
                "agent",
                "run",
                "--agent",
                "vault-smoke",
                "--thread",
                &second,
            ],
        )
        .await?,
    )?;
    #[derive(Deserialize)]
    struct RpcMethod {
        method: String,
    }
    let initialization_tokens = f
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| {
            r.url.path() == "/mcp/"
                && r.body_json::<RpcMethod>()
                    .is_ok_and(|rpc| rpc.method == "initialize")
        })
        .filter_map(|r| {
            r.headers
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_owned())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        initialization_tokens,
        [
            "Bearer initial-token",
            "Bearer initial-token",
            "Bearer refreshed-token",
            "Bearer initial-token"
        ]
    );
    f.cli(&["vault", "secret", "delete", "second", "workspace"])
        .await?;
    let revoked = chat(
        &f,
        &[
            "agent",
            "run",
            "--agent",
            "vault-smoke",
            "--thread",
            &second,
        ],
    )
    .await?;
    assert!(!revoked.status.success());
    assert!(String::from_utf8_lossy(&revoked.stderr).contains("secret is unavailable"));
    started(
        chat(
            &f,
            &["agent", "run", "--agent", "vault-smoke", "--thread", &first],
        )
        .await?,
    )?;
    Mock::given(method("POST")).and(path("/public")).respond_with(|request: &wiremock::Request| {
        assert!(request.headers.get("authorization").is_none());
        #[derive(Deserialize)]
        struct Rpc { id: Option<u64>, method: String }
        let rpc: Rpc = request.body_json().unwrap();
        let result = match rpc.method.as_str() {
            "initialize" => json!({"protocolVersion":"2025-11-25", "capabilities":{"tools":{}}, "serverInfo":{"name":"public","version":"1"}}),
            "notifications/initialized" => return ResponseTemplate::new(202),
            "tools/list" => json!({"tools":[{"name":"public_search","inputSchema":{"type":"object","properties":{}}}]}),
            other => panic!("unexpected public request: {other}"),
        };
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0", "id":rpc.id, "result":result}))
    }).mount(&f.server).await;
    let public = f.temp.path().join("public.md");
    std::fs::write(
        &public,
        std::fs::read_to_string(&file)?.replace("/mcp/", "/public"),
    )?;
    started(
        chat(
            &f,
            &["agent", "run", "--agent-file", public.to_str().unwrap()],
        )
        .await?,
    )?;
    f.cli(&["agent", "delete", "vault-smoke"]).await?;
    f.cli(&["vault", "delete", "second"]).await?;
    Ok(())
}
