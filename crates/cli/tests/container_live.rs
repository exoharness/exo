#![cfg(target_os = "macos")]

mod support;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::json;
use std::{process::Stdio, time::Duration};
use support::{Fixture, success, thread_slug};
use tokio::io::AsyncWriteExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn live(f: &Fixture, args: &[&str], input: &str) -> Result<String> {
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
        .write_all(input.as_bytes())
        .await?;
    let output =
        success(tokio::time::timeout(Duration::from_secs(180), child.wait_with_output()).await??)?;
    eprintln!("{output}");
    Ok(output)
}

async fn shell(thread: &dyn exoharness::ThreadHandle, sandbox: &str, command: &str) -> Result<i32> {
    thread
        .run_in_sandbox(exoharness::RunInSandboxRequest {
            id: sandbox.to_owned(),
            command: vec!["sh".into(), "-c".into(), command.into()],
            env: Default::default(),
        })
        .await?
        .into_parts()
        .wait
        .await
}

async fn check_resources(sandbox: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct Container {
        configuration: Configuration,
    }
    #[derive(Deserialize)]
    struct Configuration {
        labels: std::collections::HashMap<String, String>,
        resources: Resources,
    }
    #[derive(Deserialize)]
    struct Resources {
        cpus: u8,
        #[serde(rename = "memoryInBytes")]
        memory: u64,
    }
    let output = tokio::process::Command::new("container")
        .args(["list", "--format", "json"])
        .output()
        .await?;
    let containers: Vec<Container> = serde_json::from_str(&success(output)?)?;
    let config = containers
        .into_iter()
        .map(|c| c.configuration)
        .find(|c| {
            c.labels
                .get("exo.sandbox.key")
                .is_some_and(|key| key == sandbox)
        })
        .context("container not found")?;
    ensure!(
        config.resources.cpus == 2 && config.resources.memory == 2048 * 1024 * 1024,
        "container did not receive requested resources"
    );
    Ok(())
}

async fn with_live_fixture(
    backend: exoharness::SandboxProvider,
    run: impl AsyncFn(&Fixture) -> Result<()>,
) -> Result<()> {
    for provider in ["local", "remote"] {
        eprintln!("Live workflow: {backend}, {provider}");
        let f = Fixture::with_sandbox(backend.clone()).await?;
        let result = async {
            f.cli(&["provider", "switch", provider]).await?;
            run(&f).await
        }
        .await;
        let cleanup = async {
            for agent in f.runtime.exoharness_handle().list_agents().await? {
                f.runtime
                    .exoharness_handle()
                    .delete_agent(&agent.record().id)
                    .await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let shutdown = f.stop().await;
        if let Err(error) = &result {
            eprintln!("Live workflow failed: {error:#}");
        }
        if let Err(error) = &cleanup {
            eprintln!("Live cleanup failed: {error:#}");
        }
        result?;
        cleanup?;
        shutdown?;
    }
    Ok(())
}

#[actix_web::test]
#[ignore = "requires OPENAI_API_KEY, Apple container, and exo-pi-sandbox:latest"]
async fn pi_managed_local_and_http() -> Result<()> {
    let api_key = std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY is required")?;
    with_live_fixture(exoharness::SandboxProvider::AppleContainer, async |f| {
            success(f.command(&["--provider", "local", "vault", "secret", "create", "global", "live-openai", "--token-env", "LIVE_OPENAI_API_KEY"]).env("LIVE_OPENAI_API_KEY", &api_key).output().await?)?;
            f.cli(&["--provider", "local", "model", "create", "gpt-5-mini", "--secret", "live-openai"]).await?;
            let mcp = MockServer::start().await;
            for verb in ["GET", "DELETE"] {
                Mock::given(method(verb)).and(path("/mcp")).respond_with(ResponseTemplate::new(405)).mount(&mcp).await;
            }
            Mock::given(method("POST")).and(path("/mcp")).respond_with(|request: &wiremock::Request| {
                #[derive(Deserialize)]
                struct Rpc { id: Option<u64>, method: String }
                let rpc: Rpc = request.body_json().unwrap();
                let result = match rpc.method.as_str() {
                    "initialize" => json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"pi-smoke","version":"1"}}),
                    "notifications/initialized" => return ResponseTemplate::new(202),
                    "tools/list" => json!({"tools":[{"name":"fetch_code","description":"Fetch the verification code to save in proof.txt.","inputSchema":{"type":"object","properties":{},"required":[],"additionalProperties":false}}]}),
                    "tools/call" => json!({"content":[{"type":"text","text":"exo-pi-verified-47"}],"isError":false}),
                    other => panic!("unexpected MCP request: {other}"),
                };
                ResponseTemplate::new(200).insert_header("mcp-session-id","pi-session").set_body_json(json!({"jsonrpc":"2.0","id":rpc.id,"result":result}))
            }).mount(&mcp).await;
            std::fs::write(&f.agent_file, format!("---\nname: pi-live\nharness: pi\nconfig:\n  model: gpt-5-mini\npermission_policy: {{type: always_ask}}\nmcp_servers:\n  - type: url\n    name: verifier\n    url: {}/mcp\n---\nStart final replies with PI-READY. Use the requested tools and report their real results.\n", mcp.uri()))?;
            let environment = f.temp.path().join("pi.yaml");
            std::fs::write(&environment, "name: pi\nconfig:\n  provider: apple_container\n  image: exo-pi-sandbox:latest\n  default_workdir: /home/exo/workspace\n  resources: {vcpu_count: 2, memory_mib: 2048}\n  policy:\n    networking: {type: unrestricted}\n  idle_seconds: 600\n")?;
            f.cli(&["environment", "create", "pi", "--file", environment.to_str().unwrap()]).await?;
            f.cli(&["agent", "create", "pi-live", "--file", f.agent_file.to_str().unwrap()]).await?;
            let first = live(f, &["agent", "run", "--agent", "pi-live", "--environment", "pi", "--prompt", "Call the verifier fetch_code tool. Use the native bash tool to write exactly its returned code to proof.txt in your working directory. Report the code and working directory."], &"a\n".repeat(20)).await?;
            ensure!(first.contains("/home/exo/workspace"), "environment working directory was ignored");
            ensure!(first.contains("PI-READY") && first.contains("exo-pi-verified-47"), "instructions or MCP result missing");
            ensure!(first.contains("Permission required: pi.bash") && first.contains("Permission required: exo_mcp__verifier__fetch_code"), "native/MCP approvals missing");
            ensure!(!first.contains("tokens: unavailable") && !first.contains("cost: unavailable"), "usage missing");
            let slug = thread_slug(&first)?;
            let agent = exo_managed_agents::find_agent(f.runtime.exoharness_handle().as_ref(), "pi-live").await?;
            let thread = exo_managed_agents::find_thread(agent.as_ref(), slug).await?;
            let sandbox = thread.list_sandboxes().await?[0].id.clone();
            check_resources(&sandbox).await?;
            let resumed = live(f, &["agent", "run", "--agent", "pi-live", "--thread", slug, "--prompt", "Use native read to inspect proof.txt again, then report its contents. Do not rewrite it."], &"y\n".repeat(20)).await?;
            ensure!(resumed.contains("exo-pi-verified-47") && resumed.contains("Permission required: pi.read"), "saved file/approval missing");
            ensure!(thread.list_sandboxes().await?[0].id == sandbox, "resuming replaced the sandbox");
            let denied = live(f, &["agent", "run", "--agent", "pi-live", "--thread", slug, "--prompt", "Use bash to write denied.txt. If denied, stop and report that."], &"n\n".repeat(20)).await?;
            ensure!(denied.contains("Permission required:"), "denial was not requested");
            ensure!(denied.contains("← pi.bash ✗ error"), "denied tool was displayed as successful");
            let output = thread.run_in_sandbox(exoharness::RunInSandboxRequest { id: sandbox.clone(), command: vec!["sh".into(), "-c".into(), "test ! -e denied.txt && test \"$(cat proof.txt)\" = exo-pi-verified-47".into()], env: Default::default() }).await?;
            ensure!(output.into_parts().wait.await? == 0, "denied tool ran or saved file disappeared");
            let mut child = f.command(&["agent", "run", "--agent", "pi-live", "--thread", slug, "--prompt", "Run this exact native bash command: touch cancel.started; sleep 113; touch cancel.finished. Wait for it to finish."])
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
            child.stdin.take().context("stdin")?.write_all("y\n".repeat(20).as_bytes()).await?;
            tokio::time::timeout(Duration::from_secs(90), async {
                while shell(thread.as_ref(), &sandbox, "test -e cancel.started").await? != 0 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Ok::<_, anyhow::Error>(())
            }).await.context("Pi did not start the cancellation test tool")??;
            ensure!(tokio::process::Command::new("kill").args(["-INT", &child.id().context("CLI process")?.to_string()]).status().await?.success(), "failed to interrupt CLI");
            let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await??;
            ensure!(!output.status.success() && String::from_utf8_lossy(&output.stderr).contains("turn interrupted"), "Pi did not cancel: {}", String::from_utf8_lossy(&output.stderr));
            ensure!(shell(thread.as_ref(), &sandbox, "test ! -e cancel.finished && ! pgrep -f '^sleep 113$'").await? == 0, "cancelled tool kept running");
            let followup = live(f, &["agent", "run", "--agent", "pi-live", "--thread", slug, "--prompt", "The previous turn was cancelled. Use native read to inspect proof.txt and report its code."], &"y\n".repeat(20)).await?;
            ensure!(followup.contains("exo-pi-verified-47"), "follow-up after cancellation failed");
            f.cli(&["agent", "create", "second", "--file", f.agent_file.to_str().unwrap()]).await?;
            let independent = live(f, &["agent", "run", "--agent", "second", "--environment", "pi", "--prompt", "Use bash to check whether proof.txt exists. Do not create it. Report the result."], &"y\n".repeat(20)).await?;
            let second_agent = exo_managed_agents::find_agent(f.runtime.exoharness_handle().as_ref(), "second").await?;
            let second_thread = exo_managed_agents::find_thread(second_agent.as_ref(), thread_slug(&independent)?).await?;
            let output = second_thread.run_in_sandbox(exoharness::RunInSandboxRequest { id: second_thread.list_sandboxes().await?[0].id.clone(), command: vec!["sh".into(), "-c".into(), "test ! -e proof.txt".into()], env: Default::default() }).await?;
            ensure!(output.into_parts().wait.await? == 0, "different agents shared an implicit sandbox");
            live(f, &["agent", "run", "--agent-file", f.agent_file.to_str().unwrap(), "--environment", "pi", "--prompt", "Reply exactly PI-READY without using tools."], "").await?;
            let history = live(f, &["agent", "run", "--agent", "pi-live", "--thread", slug], "/history\n/quit\n").await?;
            ensure!(history.contains("exo-pi-verified-47"), "history missing");
            Ok::<_, anyhow::Error>(())
    }).await
}

#[actix_web::test]
#[ignore = "requires Apple container and exo-pi-sandbox:latest"]
async fn container_environments_local_and_http() -> Result<()> {
    with_live_fixture(exoharness::SandboxProvider::AppleContainer, async |f| {
            let shared = f.temp.path().join("shared");
            std::fs::create_dir(&shared)?;
            for name in ["first", "second"] {
                f.cli(&["agent", "create", name, "--file", f.agent_file.to_str().unwrap()]).await?;
            }
            let mut ids = std::collections::HashSet::new();
            for (agent_name, workdir) in [("first", "/home/exo/workspace"), ("first", "/home/exo/.pi"), ("second", "/home/exo/workspace")] {
                let file = f.temp.path().join("environment.json");
                std::fs::write(&file, serde_json::to_vec(&json!({
                    "name": "container", "config": {
                        "provider": "apple_container", "image": "exo-pi-sandbox:latest", "default_workdir": workdir,
                        "resources": {"vcpu_count":2,"memory_mib":2048},
                        "policy": {"networking":{"type":"disabled"}},
                        "file_system_mounts": [{"host_path": shared, "mount_path":"/shared","mode":"rw"}]
                    }
                }))?)?;
                let output = live(f, &["agent", "run", "--agent", agent_name, "--environment-file", file.to_str().unwrap(), "--prompt", "Reply without using tools."], "").await?;
                let agent = exo_managed_agents::find_agent(f.runtime.exoharness_handle().as_ref(), agent_name).await?;
                let thread = exo_managed_agents::find_thread(agent.as_ref(), thread_slug(&output)?).await?;
                let id = thread.list_sandboxes().await?[0].id.clone();
                check_resources(&id).await?;
                ensure!(ids.insert(id.clone()), "environment instances shared a sandbox");
                ensure!(shell(thread.as_ref(), &id, &format!("test \"$PWD\" = '{workdir}' && test ! -e /home/exo/private-proof && touch /home/exo/private-proof")).await? == 0, "workdir or private filesystem isolation failed");
                if shared.join("shared-proof").exists() {
                    ensure!(shell(thread.as_ref(), &id, "test \"$(cat /shared/shared-proof)\" = shared").await? == 0, "explicit mount not shared");
                } else {
                    ensure!(shell(thread.as_ref(), &id, "printf shared > /shared/shared-proof").await? == 0, "could not write shared mount");
                    ensure!(std::fs::read_to_string(shared.join("shared-proof"))? == "shared", "mount not visible to host");
                }
                ensure!(shell(thread.as_ref(), &id, "! curl -s --connect-timeout 2 --max-time 3 https://1.1.1.1").await? == 0, "disabled network allowed external access");
                live(f, &["agent", "run", "--agent", agent_name, "--thread", thread_slug(&output)?, "--prompt", "Resume without tools."], "").await?;
                ensure!(thread.list_sandboxes().await?[0].id == id, "resume replaced the sandbox");
                ensure!(shell(thread.as_ref(), &id, "test -e /home/exo/private-proof").await? == 0, "resume lost sandbox files");
            }
            Ok::<_, anyhow::Error>(())
    }).await
}

#[actix_web::test]
#[ignore = "requires OPENAI_API_KEY and the Codex image in Apple container and Docker"]
async fn codex_credentials_stay_out_of_containers() -> Result<()> {
    agent_credentials_stay_out_of_containers(
        "codex",
        "exo-codex-sandbox:latest",
        "gpt-5-mini",
        "OPENAI_API_KEY",
    )
    .await
}

#[actix_web::test]
#[ignore = "requires ANTHROPIC_API_KEY and the Claude image in Apple container and Docker"]
async fn claude_credentials_stay_out_of_containers() -> Result<()> {
    agent_credentials_stay_out_of_containers(
        "claude-code",
        "exo-claude-code-sandbox:latest",
        "claude-sonnet-4-6",
        "ANTHROPIC_API_KEY",
    )
    .await
}

#[actix_web::test]
#[ignore = "requires OPENAI_API_KEY and the Pi image in Apple container and Docker"]
async fn pi_credentials_stay_out_of_containers() -> Result<()> {
    agent_credentials_stay_out_of_containers(
        "pi",
        "exo-pi-sandbox:latest",
        "gpt-5-mini",
        "OPENAI_API_KEY",
    )
    .await
}

async fn agent_credentials_stay_out_of_containers(
    harness: &str,
    image: &str,
    model: &str,
    variable: &str,
) -> Result<()> {
    agent_credential_workflow(
        &[
            exoharness::SandboxProvider::AppleContainer,
            exoharness::SandboxProvider::Docker,
        ],
        harness,
        image,
        model,
        variable,
    )
    .await
}

#[cfg(feature = "firecracker")]
#[actix_web::test]
#[ignore = "requires OPENAI_API_KEY, Firecracker artifacts, and a rootfs containing Codex"]
async fn firecracker_codex_credentials() -> Result<()> {
    agent_credential_workflow(
        &[exoharness::SandboxProvider::Firecracker],
        "codex",
        &exoharness::default_firecracker_image(),
        "gpt-5-mini",
        "OPENAI_API_KEY",
    )
    .await
}

#[cfg(feature = "firecracker")]
#[actix_web::test]
#[ignore = "requires ANTHROPIC_API_KEY, Firecracker artifacts, and a rootfs containing Claude Code"]
async fn firecracker_claude_credentials() -> Result<()> {
    agent_credential_workflow(
        &[exoharness::SandboxProvider::Firecracker],
        "claude-code",
        &exoharness::default_firecracker_image(),
        "claude-sonnet-4-6",
        "ANTHROPIC_API_KEY",
    )
    .await
}

#[cfg(feature = "firecracker")]
#[actix_web::test]
#[ignore = "requires OPENAI_API_KEY, Firecracker artifacts, and a rootfs containing Pi"]
async fn firecracker_pi_credentials() -> Result<()> {
    agent_credential_workflow(
        &[exoharness::SandboxProvider::Firecracker],
        "pi",
        &exoharness::default_firecracker_image(),
        "gpt-5-mini",
        "OPENAI_API_KEY",
    )
    .await
}

async fn agent_credential_workflow(
    backends: &[exoharness::SandboxProvider],
    harness: &str,
    image: &str,
    model: &str,
    variable: &str,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    let inspect = r#"
const fs = require('node:fs');
const crypto = require('node:crypto');
const [variable, digest] = process.argv.slice(1);
if (!process.env[variable]?.startsWith('exo_egress_')) throw Error('model credential was not replaced');
function inspect(path) {
  let data;
  try { data = fs.readFileSync(path, 'utf8'); }
  catch (e) { if (e.code === 'ENOENT' || e.code === 'ESRCH' || (e.code === 'EACCES' && path.startsWith('/proc/'))) return; throw e; }
  for (const value of data.match(/[A-Za-z0-9_-]{32,}/g) ?? []) {
    if (crypto.createHash('sha256').update(value).digest('hex') === digest) throw Error('raw credential found in ' + path);
  }
}
function walk(path) {
  if (!fs.existsSync(path)) return;
  for (const entry of fs.readdirSync(path, { withFileTypes: true })) {
    const child = path + '/' + entry.name;
    if (entry.isDirectory()) walk(child);
    else if (entry.isFile()) inspect(child);
  }
}
for (const pid of fs.readdirSync('/proc').filter(name => /^\d+$/.test(name))) {
  inspect('/proc/' + pid + '/environ');
  inspect('/proc/' + pid + '/cmdline');
}
for (const dir of ['/tmp/exo-codex-home', '/home/exo/.codex', '/home/exo/.claude', '/home/exo/.pi']) walk(dir);
"#;
    let key = std::env::var(variable).with_context(|| format!("{variable} is required"))?;
    let digest = format!("{:x}", Sha256::digest(key.as_bytes()));
    for backend in backends {
        eprintln!("Credential workflow: {backend}, {harness}");
        let network = if *backend == exoharness::SandboxProvider::Firecracker {
            let host = if variable == "ANTHROPIC_API_KEY" {
                "api.anthropic.com"
            } else {
                "api.openai.com"
            };
            format!("{{type: limited, allowed_hosts: [{host}]}}")
        } else {
            "{type: unrestricted}".into()
        };
        with_live_fixture(backend.clone(), async |f| {
                    success(f.command(&["--provider", "local", "vault", "secret", "create", "global", "live-model", "--token-env", "LIVE_MODEL_KEY"]).env("LIVE_MODEL_KEY", &key).output().await?)?;
                    f.cli(&["--provider", "local", "model", "create", model, "--secret", "live-model"]).await?;
                    std::fs::write(&f.agent_file, format!("---\nname: credential-test\nharness: {harness}\nconfig:\n  model: {model}\n---\nFollow the user request.\n"))?;
                    let environment = f.temp.path().join("environment.yaml");
                    std::fs::write(&environment, format!("name: credential-test\nconfig:\n  provider: {backend}\n  image: {image}\n  default_workdir: /home/exo/workspace\n  resources: {{vcpu_count: 2, memory_mib: 2048}}\n  policy:\n    networking: {network}\n  idle_seconds: 600\n"))?;
                    f.cli(&["agent", "create", "credential-test", "--file", f.agent_file.to_str().context("agent path")?]).await?;
                    let first = live(f, &["agent", "run", "--agent", "credential-test", "--environment-file", environment.to_str().context("environment path")?, "--prompt", "Reply exactly CREDENTIAL-PROXY-READY without using tools."], "").await?;
                    ensure!(first.contains("CREDENTIAL-PROXY-READY"), "agent response missing");
                    ensure!(!first.contains("Reconnecting..."), "agent retried an unsupported transport");
                    ensure!(!first.contains("tokens: unavailable"), "agent usage missing");
                    let slug = thread_slug(&first)?;
                    let resumed = live(f, &["agent", "run", "--agent", "credential-test", "--thread", slug, "--prompt", "Repeat your previous response exactly. Do not use tools."], "").await?;
                    ensure!(resumed.contains("CREDENTIAL-PROXY-READY"), "saved thread did not resume");
                    let agent = exo_managed_agents::find_agent(f.runtime.exoharness_handle().as_ref(), "credential-test").await?;
                    let thread = exo_managed_agents::find_thread(agent.as_ref(), slug).await?;
                    let sandbox = thread.list_sandboxes().await?[0].id.clone();
                    let audit = thread.run_in_sandbox(exoharness::RunInSandboxRequest {
                        id: sandbox,
                        command: vec!["node".into(), "-e".into(), inspect.into(), variable.into(), digest.clone()],
                        env: std::collections::HashMap::from([(variable.into(), "caller-supplied-value".into())]),
                    }).await?;
                    ensure!(audit.into_parts().wait.await? == 0, "credential containment audit failed");
                    Ok::<_, anyhow::Error>(())
        }).await?;
    }
    Ok(())
}

#[actix_web::test]
#[ignore = "requires OPENAI_API_KEY, Apple container, and DeepWiki network access"]
async fn codex_native_mcp_local_and_http() -> Result<()> {
    native_mcp_workflow(
        "codex",
        "gpt-5.6-sol",
        "OPENAI_API_KEY",
        "exo-codex-sandbox:latest",
    )
    .await
}

#[actix_web::test]
#[ignore = "requires ANTHROPIC_API_KEY, Apple container, and DeepWiki network access"]
async fn claude_native_mcp_local_and_http() -> Result<()> {
    native_mcp_workflow(
        "claude-code",
        "claude-sonnet-4-6",
        "ANTHROPIC_API_KEY",
        "exo-claude-code-sandbox:latest",
    )
    .await
}

async fn native_mcp_workflow(
    harness: &str,
    model: &str,
    variable: &str,
    image: &str,
) -> Result<()> {
    let key = std::env::var(variable).with_context(|| format!("{variable} is required"))?;
    with_live_fixture(exoharness::SandboxProvider::AppleContainer, async |f| {
            success(f.command(&["--provider", "local", "vault", "secret", "create", "global", "native-key", "--token-env", "NATIVE_API_KEY"]).env("NATIVE_API_KEY", &key).output().await?)?;
            f.cli(&["--provider", "local", "model", "create", model, "--secret", "native-key"]).await?;
            std::fs::write(&f.agent_file, format!("---\nname: native-mcp\nharness: {harness}\nconfig:\n  model: {model}\nmcp_servers:\n  - type: url\n    name: wiki\n    url: https://mcp.deepwiki.com/mcp\n    allowed_tools: [read_wiki_structure]\n---\nUse the requested MCP tool. If permission is denied, stop; do not use another tool or retry.\n"))?;
            let environment = f.temp.path().join("native.yaml");
            std::fs::write(&environment, format!("name: native\nconfig:\n  provider: apple_container\n  image: {image}\n  default_workdir: /home/exo/workspace\n  resources: {{vcpu_count: 2, memory_mib: 2048}}\n  policy:\n    networking: {{type: unrestricted}}\n"))?;
            f.cli(&["agent", "create", "native-mcp", "--file", f.agent_file.to_str().context("agent path")?]).await?;
            let prompt = if harness == "codex" {
                "Use Promise.all to call wiki read_wiki_structure twice in parallel, once for repoName openai/openai-python and once for repoName rust-lang/rust, then report the first section title from each result. Do not use shell, web search, or any other tools."
            } else {
                "Call wiki read_wiki_structure for repoName openai/openai-python once, then report the first section title. Do not use shell, web search, or any other tools."
            };
            let first = live(f, &["agent", "run", "--agent", "native-mcp", "--environment-file", environment.to_str().context("environment path")?, "--prompt", prompt], &"y\n".repeat(20)).await?;
            ensure!(first.contains("Permission required: exo_mcp__wiki__read_wiki_structure"), "native MCP bypassed Harness approval");
            ensure!(first.contains("← exo_mcp__wiki__read_wiki_structure ✓"), "native MCP did not succeed");
            if harness == "codex" {
                ensure!(first.matches("← exo_mcp__wiki__read_wiki_structure ✓").count() == 2, "parallel MCP calls did not both succeed");
                ensure!(first.matches("Permission required: exo_mcp__wiki__read_wiki_structure").count() == 2, "each parallel MCP call must request approval");
            }
            let slug = thread_slug(&first)?;
            let denied = live(f, &["agent", "run", "--agent", "native-mcp", "--thread", slug, "--prompt", prompt], &"n\n".repeat(20)).await?;
            ensure!(denied.contains("Permission required: exo_mcp__wiki__read_wiki_structure"), "resumed native MCP bypassed approval");
            ensure!(denied.contains("← exo_mcp__wiki__read_wiki_structure ✗"), "denied native MCP was reported as successful");
            Ok::<_, anyhow::Error>(())
    }).await
}
