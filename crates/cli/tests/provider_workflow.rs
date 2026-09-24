mod support;

use anyhow::Result;
use support::{Fixture, success, thread_slug};

fn mutation_output(output: std::process::Output, provider: &str) -> Result<String> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with(&format!("provider: {provider}; account: ")),
        "{stderr}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("provider:"));
    success(output)
}

#[actix_web::test]
async fn local_and_http_cli_workflows() -> Result<()> {
    for provider in ["local", "remote"] {
        let f = Fixture::new().await?;
        let file = f.agent_file.to_str().unwrap();
        f.cli(&["provider", "switch", provider]).await?;
        let created = mutation_output(
            f.output(&["agent", "create", "saved", "--file", file], None, None)
                .await?,
            provider,
        )?;
        assert!(created.contains("created agent saved"), "{created}");
        assert!(f.cli(&["agent", "list"]).await?.contains("saved"));
        assert!(
            f.cli(&["agent", "get", "saved"])
                .await?
                .contains("gpt-5-mini")
        );
        if provider == "remote" {
            for (name, source) in [(
                "wrong-harness",
                support::SOURCE.replace("harness: basic", "harness: unknown"),
            )] {
                let invalid = f.temp.path().join(format!("{name}.md"));
                std::fs::write(&invalid, source)?;
                assert!(
                    !f.output(
                        &["agent", "create", name, "--file", invalid.to_str().unwrap()],
                        None,
                        None
                    )
                    .await?
                    .status
                    .success()
                );
                assert!(!f.cli(&["agent", "list"]).await?.contains(name));
            }
        }
        let first = mutation_output(
            f.output(&["run", "--agent", "saved", "first input"], None, None)
                .await?,
            provider,
        )?;
        assert!(first.contains("Workflow reply."), "{provider}: {first}");
        let thread = thread_slug(&first)?;
        let resumed = f
            .cli(&[
                "run",
                "--agent",
                "saved",
                "--thread",
                thread,
                "second input",
            ])
            .await?;
        assert_eq!(thread_slug(&resumed)?, thread);
        assert!(resumed.contains("Workflow reply."), "{resumed}");
        assert!(f.cli(&["thread", "list", "saved"]).await?.contains(thread));
        let history = success(
            f.output(
                &["chat", "--agent", "saved", "--thread", thread],
                None,
                Some("/history\n/quit\n"),
            )
            .await?,
        )?;
        assert!(
            history.contains("first input") && history.contains("second input"),
            "{history}"
        );
        let shown = f.cli(&["thread", "get", "saved", thread]).await?;
        assert!(shown.contains("message_count: 4"), "{shown}");
        let events = f.cli(&["thread", "events", "saved", thread]).await?;
        assert!(events.contains("turn_ended"), "{events}");
        let temporary = f
            .cli(&["run", "--agent-file", file, "temporary input"])
            .await?;
        assert!(
            temporary.contains("state: temporary") && temporary.contains("Workflow reply."),
            "{temporary}"
        );
        let listed = f.cli(&["agent", "list"]).await?;
        assert!(
            !listed.contains("workflow-agent-"),
            "temporary agent leaked: {listed}"
        );
        mutation_output(
            f.output(&["thread", "delete", "saved", thread], None, None)
                .await?,
            provider,
        )?;
        assert!(!f.cli(&["thread", "list", "saved"]).await?.contains(thread));
        mutation_output(
            f.output(&["agent", "delete", "saved"], None, None).await?,
            provider,
        )?;
        if provider == "local" {
            for action in ["create", "delete"] {
                mutation_output(
                    f.output(&["vault", action, "notice-test"], None, None)
                        .await?,
                    provider,
                )?;
            }
        }
        assert!(!f.cli(&["agent", "list"]).await?.contains("saved"));
        f.stop().await?;
    }
    Ok(())
}

#[actix_web::test]
async fn provider_crud_defaults_and_aliases_survive_cli_restarts() -> Result<()> {
    let f = Fixture::new().await?;
    let directory = f.temp.path().join("project");
    let nested = directory.join("nested");
    std::fs::create_dir_all(&nested)?;
    f.cli(&["provider", "switch", "remote"]).await?;
    let listed = f.cli(&["agent", "list"]).await?;
    assert!(!listed.contains("provider:"));
    assert!(!f.contexts.lock().unwrap().is_empty());
    f.contexts.lock().unwrap().clear();
    success(
        f.output(
            &["provider", "switch", "local", "--local"],
            Some(&directory),
            None,
        )
        .await?,
    )?;
    success(f.output(&["agent", "list"], Some(&nested), None).await?)?;
    assert!(f.contexts.lock().unwrap().is_empty());
    mutation_output(
        f.output(
            &["vault", "create", "directory-notice"],
            Some(&nested),
            None,
        )
        .await?,
        "local",
    )?;
    success(
        f.output(
            &["--provider", "remote", "agent", "list"],
            Some(&nested),
            None,
        )
        .await?,
    )?;
    assert!(!f.contexts.lock().unwrap().is_empty());
    for args in [
        vec!["provider", "create", "remote", "--url", &f.endpoint],
        vec!["provider", "update", "missing", "--url", &f.endpoint],
    ] {
        assert!(!f.output(&args, None, None).await?.status.success());
    }
    f.cli(&[
        "agent",
        "create",
        "pinned",
        "--file",
        f.agent_file.to_str().unwrap(),
    ])
    .await?;
    let first = f.cli(&["run", "--agent", "pinned", "hello"]).await?;
    let thread = thread_slug(&first)?;
    f.cli(&["provider", "switch", "local"]).await?;
    f.contexts.lock().unwrap().clear();
    success(
        f.output(&["agent", "get", "pinned"], Some(&nested), None)
            .await?,
    )?;
    assert!(!f.contexts.lock().unwrap().is_empty());
    let conflict = f
        .output(
            &["--provider", "local", "agent", "get", "pinned"],
            None,
            None,
        )
        .await?;
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("alias belongs to provider remote"));
    f.cli(&[
        "provider",
        "update",
        "remote",
        "--url",
        &format!("{}/", f.endpoint),
    ])
    .await?;
    f.cli(&["thread", "get", "pinned", thread]).await?;
    f.cli(&[
        "provider",
        "update",
        "remote",
        "--url",
        "http://127.0.0.1:1/exo",
    ])
    .await?;
    for args in [
        vec!["agent", "get", "pinned"],
        vec!["thread", "get", "pinned", thread],
    ] {
        let changed = f.output(&args, None, None).await?;
        assert!(!changed.status.success());
        assert!(
            String::from_utf8_lossy(&changed.stderr)
                .contains("provider configuration changed for this alias")
        );
    }
    f.cli(&["provider", "update", "remote", "--url", &f.endpoint])
        .await?;
    f.cli(&["thread", "get", "pinned", thread]).await?;
    f.cli(&[
        "provider",
        "update",
        "remote",
        "--url",
        &format!("{}?workspace=other", f.endpoint),
    ])
    .await?;
    assert!(
        !f.output(&["agent", "get", "pinned"], None, None)
            .await?
            .status
            .success()
    );
    f.cli(&["provider", "delete", "remote"]).await?;
    assert!(!f.cli(&["provider", "list"]).await?.contains("remote"));
    let missing = f.output(&["agent", "get", "pinned"], None, None).await?;
    assert!(String::from_utf8_lossy(&missing.stderr).contains("provider remote is not configured"));
    f.cli(&[
        "provider",
        "create",
        "remote",
        "--url",
        &f.endpoint,
        "--api-key-env",
        "RUNTIME_TOKEN",
    ])
    .await?;
    f.cli(&["agent", "get", "pinned"]).await?;
    f.cli(&["provider", "login", "remote"]).await?;
    assert!(
        f.cli(&["provider", "get", "remote"])
            .await?
            .contains("\"account_id\": \"local\"")
    );
    f.cli(&[
        "provider",
        "update",
        "remote",
        "--api-key-env",
        "ROTATED_TOKEN",
    ])
    .await?;
    success(
        f.command(&["thread", "get", "pinned", thread])
            .env("ROTATED_TOKEN", "workflow-token")
            .output()
            .await?,
    )?;
    f.cli(&["provider", "delete", "local"]).await?;
    assert!(!f.cli(&["provider", "list"]).await?.contains("state"));
    let config = std::fs::read_to_string(f.temp.path().join("config/providers.json"))?;
    #[derive(serde::Deserialize)]
    struct Defaults {
        default: Option<String>,
        directory_defaults: std::collections::BTreeMap<String, String>,
    }
    let config: Defaults = serde_json::from_str(&config)?;
    assert!(config.default.is_none() && config.directory_defaults.is_empty());
    f.stop().await
}

async fn model_started(f: &Fixture, marker: &str) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if f.model
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| String::from_utf8_lossy(&request.body).contains(marker))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await?;
    Ok(())
}

#[actix_web::test]
async fn local_and_http_live_cancellation_finalizes_and_allows_resume() -> Result<()> {
    use std::{process::Stdio, time::Duration};
    use wiremock::{
        Mock,
        matchers::{body_string_contains, method, path},
    };
    for provider in ["local", "remote"] {
        let f = Fixture::new().await?;
        f.cli(&["provider", "switch", provider]).await?;
        f.cli(&[
            "agent",
            "create",
            "saved",
            "--file",
            f.agent_file.to_str().unwrap(),
        ])
        .await?;
        let delay = Mock::given(method("POST"))
            .and(path("/responses"))
            .and(body_string_contains("cancel this turn"))
            .respond_with(support::model_response().set_delay(Duration::from_secs(5)))
            .with_priority(1)
            .mount_as_scoped(&f.model)
            .await;
        let child = f
            .command(&["run", "--agent", "saved", "cancel this turn"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        model_started(&f, "cancel this turn").await?;
        let status = tokio::process::Command::new("kill")
            .args(["-INT", &child.id().unwrap().to_string()])
            .status()
            .await?;
        assert!(status.success());
        let output =
            tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await??;
        assert!(!output.status.success(), "{provider}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("turn interrupted"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout)?;
        let thread = thread_slug(&stdout)?;
        let events = f.cli(&["thread", "events", "saved", thread]).await?;
        assert!(
            events.contains("turn_ended") && events.contains("cancelled"),
            "{events}"
        );
        drop(delay);
        assert!(
            f.cli(&["run", "--agent", "saved", "--thread", thread, "try again"])
                .await?
                .contains("Workflow reply.")
        );
        f.stop().await?;
    }
    Ok(())
}

#[actix_web::test]
async fn saved_http_turn_survives_cli_disconnect_and_temporary_cleanup() -> Result<()> {
    use exo_managed_agents::http::{
        RuntimeClient,
        protocol::{EventsQuery, WatchQuery},
    };
    use futures::StreamExt;
    use std::{process::Stdio, time::Duration};
    use wiremock::{
        Mock,
        matchers::{body_string_contains, method, path},
    };
    let f = Fixture::new().await?;
    f.cli(&["provider", "switch", "remote"]).await?;
    f.cli(&[
        "agent",
        "create",
        "saved",
        "--file",
        f.agent_file.to_str().unwrap(),
    ])
    .await?;
    let delay = Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_string_contains("detached turn"))
        .respond_with(support::model_response().set_delay(Duration::from_secs(5)))
        .with_priority(1)
        .mount_as_scoped(&f.model)
        .await;
    let mut child = f
        .command(&["run", "--agent", "saved", "detached turn"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    model_started(&f, "detached turn").await?;
    child.start_kill()?;
    let output = child.wait_with_output().await?;
    let stdout = String::from_utf8(output.stdout)?;
    let slug = thread_slug(&stdout)?;
    let client = RuntimeClient::new(&f.endpoint)?.with_bearer_token("workflow-token".into());
    let agent = client.list_agents(None).await?.agents.pop().unwrap();
    let thread = client
        .list_threads(agent.id, &Default::default())
        .await?
        .threads
        .pop()
        .unwrap();
    let before = client
        .events(agent.id, thread.id, &EventsQuery::default())
        .await?;
    assert!(
        !before
            .events
            .iter()
            .any(|event| matches!(event.data, exoharness::EventData::TurnEnded))
    );
    let temporary = f
        .cli(&[
            "run",
            "--agent-file",
            f.agent_file.to_str().unwrap(),
            "short temporary turn",
        ])
        .await?;
    assert!(temporary.contains("Workflow reply."));
    let id: exoharness::AgentId = temporary
        .lines()
        .find(|line| line.starts_with("agent: "))
        .unwrap()
        .rsplit_once('(')
        .unwrap()
        .1
        .trim_end_matches(')')
        .parse()?;
    assert!(client.get_agent(id).await?.is_none());
    let mut watch = client
        .watch(
            agent.id,
            thread.id,
            &WatchQuery {
                after: before.events.last().map(|e| e.id),
            },
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = watch.next().await {
            let event = event?;
            anyhow::ensure!(
                !matches!(event.data, exoharness::EventData::Error { .. }),
                "{event:?}"
            );
            if matches!(event.data, exoharness::EventData::TurnEnded) {
                return Ok::<_, anyhow::Error>(());
            }
        }
        anyhow::bail!("turn stream ended before completion")
    })
    .await??;
    drop(watch);
    let history = success(
        f.output(
            &["chat", "--agent", "saved", "--thread", slug],
            None,
            Some("/history\n/quit\n"),
        )
        .await?,
    )?;
    assert!(
        history.contains("detached turn") && history.contains("Workflow reply."),
        "{history}"
    );
    assert_eq!(
        f.model
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains("detached turn"))
            .count(),
        1
    );
    drop(delay);
    f.stop().await
}

#[actix_web::test]
async fn local_and_http_providers_configure_named_harnesses() -> Result<()> {
    let f = Fixture::new().await?;
    for provider in ["local", "remote"] {
        f.cli(&["provider", "switch", provider]).await?;
        for harness in ["rlm", "codex", "claude-code", "cursor", "pi"] {
            let source = support::SOURCE.replace("harness: basic", &format!("harness: {harness}"));
            std::fs::write(&f.agent_file, &source)?;
            let name = format!("{provider}-{harness}");
            f.cli(&[
                "agent",
                "create",
                &name,
                "--file",
                f.agent_file.to_str().unwrap(),
            ])
            .await?;
            let state = f.runtime.exoharness_handle();
            let agent = exo_managed_agents::find_agent(state.as_ref(), &name).await?;
            let definition = exo_managed_agents::load_definition(agent.as_ref())
                .await?
                .unwrap();
            assert_eq!(definition.source(), source);
            let config = executor::load_agent_config(agent.as_ref()).await?;
            assert_eq!(
                config.harness,
                if harness == "rlm" {
                    executor::AgentHarnessKind::Rlm
                } else {
                    executor::AgentHarnessKind::TypeScript
                }
            );
            if let Some(module) = config.typescript {
                assert!(std::path::Path::new(&module.module_path).is_file());
            }
            success(
                f.output(
                    &["chat", "--harness", harness, "--agent", &name],
                    None,
                    Some("/quit\n"),
                )
                .await?,
            )?;
        }
    }
    f.stop().await
}

#[actix_web::test]
async fn remote_spec_uses_provider_harness_and_per_thread_vault_credentials() -> Result<()> {
    use serde::Deserialize;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    let mut f = Fixture::with_sandbox(exoharness::SandboxProvider::Docker).await?;
    let mcp = MockServer::start().await;
    Mock::given(method("POST")).and(path("/mcp")).respond_with(|request: &wiremock::Request| {
        let workspace = match request.headers.get("authorization").and_then(|h| h.to_str().ok()) {
            Some("Bearer server-vault-token") => "Server workspace",
            Some("Bearer other-vault-token") => "Other workspace",
            _ => return ResponseTemplate::new(401).insert_header("www-authenticate", "Bearer"),
        };
        #[derive(Deserialize)]
        struct Rpc { id: Option<u64>, method: String }
        let rpc: Rpc = request.body_json().unwrap();
        let result = match rpc.method.as_str() {
            "initialize" => json!({"protocolVersion":"2025-11-25", "capabilities":{"tools":{}}, "serverInfo":{"name":"workspace","version":"1"}}),
            "notifications/initialized" => return ResponseTemplate::new(202),
            "tools/list" => json!({"tools":[{"name":"search","inputSchema":{"type":"object","properties":{}}}]}),
            "tools/call" => json!({"content":[{"type":"text","text":workspace}],"isError":false}),
            other => panic!("unexpected MCP request {other}"),
        };
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":rpc.id,"result":result}))
    }).mount(&mcp).await;
    for verb in ["GET", "DELETE"] {
        Mock::given(method(verb))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(405))
            .mount(&mcp)
            .await;
    }
    let url = format!("{}/mcp", mcp.uri());
    for (vault, token) in [
        ("server-only", "server-vault-token"),
        ("other", "other-vault-token"),
    ] {
        f.cli(&["vault", "create", vault]).await?;
        success(
            f.command(&[
                "vault",
                "secret",
                "create",
                vault,
                "workspace",
                "--mcp-server-url",
                &url,
                "--token-env",
                "MCP_TOKEN",
            ])
            .env("MCP_TOKEN", token)
            .output()
            .await?,
        )?;
    }
    let harness_dir = tempfile::tempdir_in(std::env::current_dir()?)?;
    let module = harness_dir
        .path()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
        + "/harness.ts";
    let sdk = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../exoharness/typescript/harness/index.ts")
        .canonicalize()?;
    std::fs::write(
        harness_dir.path().join("harness.ts"),
        format!(
            r#"
import {{ defineHarness, messagesEvent }} from {};
export default defineHarness({{
  async runTurn(context) {{
    const result = await context.executeTool({{ functionName: "exo_mcp__workspace__search", arguments: {{}} }});
    const text = JSON.stringify(result);
    await context.stream.text(text);
    await context.exoharness.current.turn.addEvents([messagesEvent([{{ role: "assistant", content: text }}])]);
  }}
}});
"#,
            serde_json::to_string(&sdk)?
        ),
    )?;
    assert!(!f.temp.path().join(&module).exists());
    let source = format!(
        "---\nname: Remote workspace\nharness: {module}\nconfig:\n  model: gpt-5-mini\nmcp_servers:\n  - type: url\n    name: workspace\n    url: {url}\n---\nLook up the workspace.\n"
    );
    std::fs::write(&f.agent_file, &source)?;
    f.cli(&["provider", "switch", "remote"]).await?;
    f.root = f.temp.path().join("empty-client-state");
    f.cli(&[
        "agent",
        "create",
        "remote-workspace",
        "--file",
        f.agent_file.to_str().unwrap(),
    ])
    .await?;
    let state = f.runtime.exoharness_handle();
    let agent = exo_managed_agents::find_agent(state.as_ref(), "remote-workspace").await?;
    assert_eq!(
        exo_managed_agents::load_definition(agent.as_ref())
            .await?
            .unwrap()
            .source(),
        source
    );
    let first = f
        .cli(&[
            "run",
            "--agent",
            "remote-workspace",
            "--vault",
            "server-only",
            "first",
        ])
        .await?;
    assert!(first.contains("Server workspace"), "{first}");
    let other = f
        .cli(&[
            "run",
            "--agent",
            "remote-workspace",
            "--vault",
            "other",
            "second",
        ])
        .await?;
    assert!(other.contains("Other workspace"), "{other}");
    let resumed = f
        .cli(&[
            "run",
            "--agent",
            "remote-workspace",
            "--thread",
            thread_slug(&first)?,
            "again",
        ])
        .await?;
    assert!(resumed.contains("Server workspace"), "{resumed}");
    let temporary = f
        .cli(&[
            "run",
            "--agent-file",
            f.agent_file.to_str().unwrap(),
            "--vault",
            "other",
            "temporary",
        ])
        .await?;
    assert!(temporary.contains("Other workspace"), "{temporary}");
    assert!(
        !f.cli(&["agent", "list"])
            .await?
            .contains("remote-workspace-")
    );
    assert!(!f.root.exists(), "remote execution created local state");
    assert!(f.model.received_requests().await.unwrap().is_empty());
    success(
        f.command(&[
            "--provider",
            "local",
            "vault",
            "secret",
            "update",
            "server-only",
            "workspace",
            "--token-env",
            "MCP_TOKEN",
        ])
        .env("MCP_TOKEN", "other-vault-token")
        .output()
        .await?,
    )?;
    let rotated = f
        .cli(&[
            "run",
            "--agent",
            "remote-workspace",
            "--thread",
            thread_slug(&first)?,
            "after rotation",
        ])
        .await?;
    assert!(rotated.contains("Other workspace"), "{rotated}");
    f.cli(&[
        "--provider",
        "local",
        "vault",
        "secret",
        "delete",
        "server-only",
        "workspace",
    ])
    .await?;
    let revoked = f
        .output(
            &[
                "run",
                "--agent",
                "remote-workspace",
                "--thread",
                thread_slug(&first)?,
                "after revocation",
            ],
            None,
            None,
        )
        .await?;
    assert!(!revoked.status.success());
    assert!(
        String::from_utf8_lossy(&revoked.stderr).contains("secret"),
        "{}",
        String::from_utf8_lossy(&revoked.stderr)
    );
    agent
        .write_artifact(exoharness::WriteArtifactRequest {
            path: exo_managed_agents::AGENT_DEFINITION_PATH.into(),
            contents: source
                .replace(&url, &format!("{}/unexpected", mcp.uri()))
                .into_bytes(),
        })
        .await?;
    let changed = f
        .output(
            &[
                "run",
                "--agent",
                "remote-workspace",
                "--thread",
                thread_slug(&first)?,
                "after spec change",
            ],
            None,
            None,
        )
        .await?;
    assert!(!changed.status.success());
    assert!(
        String::from_utf8_lossy(&changed.stderr).contains("MCP configuration changed"),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    assert!(
        !mcp.received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path() == "/unexpected")
    );
    assert!(!f.root.exists());
    f.stop().await
}

#[actix_web::test]
async fn provider_urls_preserve_runtime_specific_scope() -> Result<()> {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path, query_param},
    };
    let f = Fixture::new().await?;
    let server = MockServer::start().await;
    for (route, response) in [
        (
            "identity",
            serde_json::json!({"account_id":"scoped-account"}),
        ),
        ("agent", serde_json::json!({"agents":[]})),
        ("vault", serde_json::json!([])),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/runtime/{route}")))
            .and(query_param("workspace", "team/one"))
            .and(header("authorization", "Bearer workflow-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;
    }
    let endpoint = format!("{}/runtime?workspace=team%2Fone", server.uri());
    f.cli(&[
        "provider",
        "create",
        "scoped",
        "--url",
        &endpoint,
        "--api-key-env",
        "RUNTIME_TOKEN",
    ])
    .await?;
    assert!(
        f.cli(&["provider", "get", "scoped"])
            .await?
            .contains(&endpoint)
    );
    f.cli(&[
        "provider",
        "login",
        "scoped",
        "--api-key-env",
        "RUNTIME_TOKEN",
    ])
    .await?;
    #[derive(serde::Deserialize)]
    struct Profile {
        api_key_env: Option<String>,
        account_id: Option<String>,
        stored_credentials: bool,
    }
    let profile: Profile = serde_json::from_str(&f.cli(&["provider", "get", "scoped"]).await?)?;
    assert_eq!(profile.api_key_env.as_deref(), Some("RUNTIME_TOKEN"));
    assert_eq!(profile.account_id.as_deref(), Some("scoped-account"));
    assert!(!profile.stored_credentials);
    let missing = f
        .command(&["--provider", "scoped", "agent", "list"])
        .env_remove("RUNTIME_TOKEN")
        .output()
        .await?;
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("RUNTIME_TOKEN is not set"));
    f.cli(&["--provider", "scoped", "agent", "list"]).await?;
    f.cli(&["--provider", "scoped", "vault", "list"]).await?;
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request.url.path() == "/runtime/agent")
    );
    assert!(
        requests
            .iter()
            .any(|request| request.url.path() == "/runtime/vault")
    );
    f.stop().await
}

#[actix_web::test]
async fn provider_context_persists_and_local_selection_does_not_change_the_profile() -> Result<()> {
    use std::collections::BTreeMap;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

    let f = Fixture::new().await?;
    let server = MockServer::start().await;
    f.cli(&[
        "provider",
        "create",
        "scoped",
        "--url",
        &format!("{}/exo", server.uri()),
        "--api-key-env",
        "RUNTIME_TOKEN",
        "--context",
        "org_name=profile,project_name=default",
    ])
    .await?;
    f.cli(&[
        "provider",
        "switch",
        "scoped",
        "--context",
        "org_name=global",
    ])
    .await?;
    let directory = f.temp.path().join("project");
    let nested = directory.join("nested");
    std::fs::create_dir_all(&nested)?;
    success(
        f.output(
            &[
                "provider",
                "switch",
                "scoped",
                "--local",
                "--context",
                "org_name=directory",
            ],
            Some(&directory),
            None,
        )
        .await?,
    )?;
    for (args, cwd, expected) in [
        (vec!["agent", "list"], None, "global"),
        (vec!["agent", "list"], Some(nested.as_path()), "directory"),
        (
            vec!["--provider", "scoped", "agent", "list"],
            Some(nested.as_path()),
            "profile",
        ),
    ] {
        server.reset().await;
        for (route, body) in [
            ("identity", serde_json::json!({"account_id":"alice"})),
            ("agent", serde_json::json!({"agents":[]})),
        ] {
            Mock::given(path(format!("/exo/{route}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }
        success(f.output(&args, cwd, None).await?)?;
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests {
            let context: BTreeMap<String, String> =
                serde_json::from_str(request.headers["x-exo-context"].to_str()?)?;
            assert_eq!(context["org_name"], expected);
            assert_eq!(context.contains_key("project_name"), expected == "profile");
        }
    }
    f.cli(&[
        "provider",
        "update",
        "scoped",
        "--context",
        "org_name=updated",
    ])
    .await?;
    #[derive(serde::Deserialize)]
    struct View {
        context: BTreeMap<String, String>,
        effective_context: BTreeMap<String, String>,
    }
    let shown: View = serde_json::from_str(&success(
        f.output(&["provider", "get", "scoped"], Some(&nested), None)
            .await?,
    )?)?;
    assert_eq!(
        shown.context,
        BTreeMap::from([("org_name".into(), "updated".into())])
    );
    assert_eq!(shown.effective_context["org_name"], "directory");
    success(
        f.output(
            &["provider", "switch", "scoped", "--local"],
            Some(&directory),
            None,
        )
        .await?,
    )?;
    let shown: View = serde_json::from_str(&success(
        f.output(&["provider", "get", "scoped"], Some(&nested), None)
            .await?,
    )?)?;
    assert_eq!(shown.effective_context, shown.context);
    f.cli(&["provider", "update", "scoped", "--context", ""])
        .await?;
    let shown: View = serde_json::from_str(&f.cli(&["provider", "get", "scoped"]).await?)?;
    assert!(shown.context.is_empty());
    for value in ["org_name", "=acme", "org_name=", "org_name=a,org_name=b"] {
        let output = f
            .output(
                &["provider", "update", "scoped", "--context", value],
                None,
                None,
            )
            .await?;
        assert!(!output.status.success(), "accepted invalid context {value}");
    }
    f.stop().await
}

#[actix_web::test]
async fn saved_aliases_keep_their_context_after_profile_and_selection_changes() -> Result<()> {
    let f = Fixture::new().await?;
    f.cli(&[
        "provider",
        "update",
        "remote",
        "--context",
        "workspace=original",
    ])
    .await?;
    f.cli(&["provider", "switch", "remote"]).await?;
    f.cli(&[
        "agent",
        "create",
        "pinned-context",
        "--file",
        f.agent_file.to_str().unwrap(),
    ])
    .await?;
    let first = f
        .cli(&["run", "--agent", "pinned-context", "hello"])
        .await?;
    let thread = thread_slug(&first)?;
    f.cli(&[
        "provider",
        "update",
        "remote",
        "--context",
        "workspace=other",
    ])
    .await?;
    f.cli(&[
        "provider",
        "switch",
        "remote",
        "--context",
        "workspace=selection",
    ])
    .await?;
    for args in [
        vec!["agent", "get", "pinned-context"],
        vec![
            "run",
            "--agent",
            "pinned-context",
            "--thread",
            thread,
            "resume",
        ],
    ] {
        f.cli(&args).await?;
    }
    {
        let mut contexts = f.contexts.lock().unwrap();
        assert!(
            contexts
                .iter()
                .any(|(path, _)| path.ends_with("/event/watch"))
        );
        assert!(contexts.iter().any(|(path, _)| path.ends_with("/vault")));
        for (path, context) in contexts.iter() {
            let context: std::collections::BTreeMap<String, String> =
                serde_json::from_str(context.as_ref().expect("context header").to_str()?)?;
            assert_eq!(context["workspace"], "original", "{path}");
        }
        contexts.clear();
    }
    f.cli(&["--provider", "remote", "vault", "list"]).await?;
    for (_, context) in f.contexts.lock().unwrap().iter() {
        let context: std::collections::BTreeMap<String, String> =
            serde_json::from_str(context.as_ref().expect("context header").to_str()?)?;
        assert_eq!(context["workspace"], "other");
    }
    f.stop().await
}

#[actix_web::test]
async fn provider_errors_explain_how_to_set_context_and_creation_is_offline() -> Result<()> {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
    let f = Fixture::new().await?;
    let server = MockServer::start().await;
    Mock::given(path("/exo/identity"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"account_id":"alice"})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/exo/agent"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "code": "context_required", "message": "  Select a workspace.\n\u{1b}[31m  ",
            "context": {"region": "server-suggestion", "workspace": "<workspace>"}
        })))
        .mount(&server)
        .await;
    f.cli(&[
        "provider",
        "create",
        "scoped",
        "--url",
        &format!("{}/exo", server.uri()),
        "--api-key-env",
        "RUNTIME_TOKEN",
    ])
    .await?;
    f.cli(&["provider", "update", "scoped", "--context", "region=west"])
        .await?;
    f.cli(&["provider", "switch", "scoped"]).await?;
    let nested = f.temp.path().join("nested");
    std::fs::create_dir(&nested)?;
    for (args, repair) in [
        (vec!["agent", "list"], "switch scoped"),
        (
            vec!["--provider", "scoped", "agent", "list"],
            "update scoped",
        ),
    ] {
        let output = f.output(&args, None, None).await?;
        assert!(!output.status.success());
        assert_eq!(
            String::from_utf8(output.stderr)?,
            format!(
                "Error: Select a workspace.\\n\\u{{1b}}[31m\nRun: exo provider {repair} --context 'region=west,workspace=<workspace>'\n"
            )
        );
    }
    f.cli(&[
        "provider",
        "switch",
        "scoped",
        "--local",
        "--context",
        "region=west",
    ])
    .await?;
    let output = f.output(&["agent", "list"], Some(&nested), None).await?;
    assert!(!output.status.success());
    let directory = f.temp.path().canonicalize()?;
    let directory = directory.to_string_lossy();
    assert_eq!(
        String::from_utf8(output.stderr)?,
        format!(
            "Error: Select a workspace.\\n\\u{{1b}}[31m\nRun: (cd {} && exo provider switch scoped --local --context 'region=west,workspace=<workspace>')\n",
            shlex::try_quote(&directory)?
        )
    );
    let output = f
        .output(
            &[
                "provider",
                "create",
                "offline",
                "--url",
                "http://127.0.0.1:1/exo",
            ],
            None,
            None,
        )
        .await?;
    assert!(output.status.success());
    let feedback = String::from_utf8(output.stderr)?;
    assert!(
        feedback.contains("Saved provider offline.")
            && feedback.contains("exo provider login offline"),
        "{feedback}"
    );
    f.cli(&["provider", "get", "offline"]).await?;
    let invalid = f
        .output(
            &[
                "provider",
                "create",
                "invalid",
                "--url",
                "ftp://localhost/exo",
            ],
            None,
            None,
        )
        .await?;
    assert!(!invalid.status.success());
    assert!(!f.cli(&["provider", "list"]).await?.contains("invalid"));
    f.stop().await
}
