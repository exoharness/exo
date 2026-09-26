#![cfg(target_os = "macos")]

mod support;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::json;
use std::{process::Stdio, time::Duration};
use support::{Fixture, success, thread_slug};
use tokio::io::AsyncWriteExt;

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

#[actix_web::test]
#[ignore = "requires Apple container and exo-pi-sandbox:latest"]
async fn container_environments_local_and_http() -> Result<()> {
    for provider in ["local", "remote"] {
        let f = Fixture::with_sandbox(exoharness::SandboxProvider::AppleContainer).await?;
        let result = async {
            f.cli(&["provider", "switch", provider]).await?;
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
                        "provider": "apple_container", "image": "exo-pi-sandbox:latest", "workdir": workdir,
                        "resources": {"vcpu_count":2,"memory_mib":2048},
                        "policy": {"networking":{"type":"disabled"}},
                        "file_system_mounts": [{"host_path": shared, "mount_path":"/shared","mode":"rw"}]
                    }
                }))?)?;
                let output = live(&f, &["run", "--agent", agent_name, "--environment-file", file.to_str().unwrap(), "Reply without using tools."], "").await?;
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
                live(&f, &["run", "--agent", agent_name, "--thread", thread_slug(&output)?, "Resume without tools."], "").await?;
                ensure!(thread.list_sandboxes().await?[0].id == id, "resume replaced the sandbox");
                ensure!(shell(thread.as_ref(), &id, "test -e /home/exo/private-proof").await? == 0, "resume lost sandbox files");
            }
            Ok::<_, anyhow::Error>(())
        }.await;
        if let Err(error) = &result {
            eprintln!("Container environment workflow failed: {error:#}");
        }
        let cleanup = async {
            for agent in f.runtime.exoharness_handle().list_agents().await? {
                f.runtime
                    .exoharness_handle()
                    .delete_agent(&agent.record().id)
                    .await?;
            }
            f.stop().await
        }
        .await;
        result?;
        cleanup?;
    }
    Ok(())
}
