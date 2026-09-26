use exoharness::vault::{OAuthRefresh, SecretTarget, global_vault};
use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, PutSecretRequest, SandboxBackendRegistration,
    SandboxProvider, Secret, SecretBackendChoice,
};
use serde::Deserialize;
use serde_json::json;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method},
};

#[tokio::test]
async fn vault_credentials_stay_host_side_and_typescript_preserves_oauth_refresh()
-> anyhow::Result<()> {
    #[derive(Deserialize)]
    struct Rpc {
        id: Option<u64>,
        method: String,
    }
    let server = MockServer::start().await;
    let revision = Arc::new(AtomicUsize::new(0));
    let server_revision = revision.clone();
    Mock::given(method("POST")).and(header("authorization", "Bearer selected-token"))
        .respond_with(move |request: &wiremock::Request| {
            let rpc: Rpc = request.body_json().unwrap();
            let result = match rpc.method.as_str() {
                "initialize" => json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}),
                "tools/list" => {
                    let revision = server_revision.load(Ordering::SeqCst);
                    if revision == 2 {
                        json!({"tools":[]})
                    } else {
                        json!({"tools":[{"name":"search", "description":format!("revision {revision}"), "inputSchema":{"type":"object"}}]})
                    }
                },
                _ => return ResponseTemplate::new(202),
            };
            ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":rpc.id,"result":result}))
        }).expect(18).mount(&server).await;
    let temp = TempDir::new()?;
    let root = temp.path().join("state");
    let prices = temp.path().join("prices.json");
    let module = temp.path().join("harness.mjs");
    let definition = temp.path().join("agent.md");
    let env_file = temp.path().join("test.env");
    std::fs::write(&prices, "{}")?;
    std::fs::write(&env_file, "EXO_TEST_VISIBLE=yes\n")?;
    std::fs::write(
        &module,
        r#"export default {
  async runTurn(context) {
    if (Object.values(process.env).includes("selected-token")) throw new Error("MCP token reached runner");
    if (process.env.EXO_TEST_VISIBLE !== "yes") throw new Error("ordinary environment was lost");
    const vaults = await context.exoharness.current.conversation.listVaults();
    const vault = vaults.find((vault) => vault.record.name === "global");
    const metadata = (await vault.listSecrets()).find((secret) => secret.name === "mcp");
    const secret = await vault.getSecret(metadata.id);
    await vault.updateSecret(metadata.id, secret);
    await context.stream.text("credentials stayed host-side");
  }
};"#,
    )?;
    std::fs::write(
        &definition,
        format!(
            "---\nname: test-agent\nharness: {}\nconfig:\n  model: fixture\nmcp_servers:\n  - type: url\n    name: fixture\n    url: {}\n---\nTest credentials.\n",
            module.display(),
            server.uri()
        ),
    )?;
    let command = |name: &str| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_exo"));
        cmd.arg(name)
            .current_dir(temp.path())
            .arg("--root")
            .arg(&root)
            .args(["--secret-backend", "file"])
            .arg("--master-key-path")
            .arg(temp.path().join("master.key"))
            .arg("--pricing-path")
            .arg(&prices)
            .env("EXO_TEST_VISIBLE", "yes");
        cmd
    };
    for args in [
        vec![
            "vault",
            "secret",
            "create",
            "global",
            "model-key",
            "--token-env",
            "EXO_TEST_VISIBLE",
        ],
        vec!["model", "create", "fixture", "--secret", "model-key"],
        vec![
            "agent",
            "create",
            "saved",
            "--file",
            definition.to_str().unwrap(),
        ],
        vec![
            "conversation",
            "create",
            "saved",
            "History",
            "--slug",
            "history",
        ],
    ] {
        let output = command(args[0]).args(&args[1..]).output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let storage = BasicExoHarness::new(BasicExoHarnessConfig {
        root: root.join("exoharness"),
        secret_backend: SecretBackendChoice::File {
            path: Some(temp.path().join("master.key")),
        },
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_policy: None,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
    })
    .await?;
    let vault = global_vault(&storage).await?;
    let secret = Secret::Oauth {
        access_token: "selected-token".into(),
        refresh_token: Some("refresh-token".into()),
        expires_at: Some(4_000_000_000),
        refresh: Some(OAuthRefresh {
            token_endpoint: format!("{}/token", server.uri()),
            client_id: "fixture".into(),
            resource: Some(server.uri()),
            scopes: vec!["read".into()],
        }),
    };
    let id = vault
        .put_secret(PutSecretRequest {
            name: "mcp".into(),
            target: Some(SecretTarget::mcp(&server.uri())?),
            secret: secret.clone(),
        })
        .await?;
    for (saved, version, expected_inventories) in [
        (false, 0, 0),
        (true, 0, 1),
        (true, 0, 1),
        (true, 1, 2),
        (true, 2, 3),
        (true, 2, 3),
    ] {
        revision.store(version, Ordering::SeqCst);
        let mut cmd = command("agent");
        cmd.arg("run");
        if saved {
            cmd.arg("--env-file").arg(&env_file);
        }
        cmd.arg(if saved { "--agent" } else { "--agent-file" })
            .arg(if saved {
                "saved"
            } else {
                definition.to_str().unwrap()
            })
            .args(["--prompt", "test"]);
        if saved {
            cmd.args(["--thread", "history"]);
        }
        let output = cmd.output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert_eq!(vault.get_secret(&id).await?, Some(secret.clone()));
        assert!(
            stdout.contains("credentials stayed host-side"),
            "{stdout}\n{stderr}"
        );
        if saved {
            let output = command("conversation")
                .args(["events", "saved", "history", "--type", "mcp_tools"])
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let events: exoharness::GetEventsResult = serde_json::from_slice(&output.stdout)?;
            assert_eq!(events.events.len(), expected_inventories);
            let exoharness::EventData::Custom { payload, .. } = &events.events.last().unwrap().data
            else {
                panic!("expected MCP inventory");
            };
            let tools: Vec<exo_mcp::McpTool> = serde_json::from_value(payload.clone())?;
            if version == 2 {
                assert!(tools.is_empty());
            } else {
                assert_eq!(tools[0].description, format!("revision {version}"));
            }
        }
    }
    Ok(())
}
