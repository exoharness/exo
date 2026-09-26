#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

use anyhow::{Context, Result, ensure};
use tempfile::TempDir;

fn executable(path: &Path, source: &str) -> Result<()> {
    fs::write(path, source)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn quote(path: &Path) -> Result<String> {
    Ok(shlex::try_quote(path.to_str().context("test path")?)?.into_owned())
}

fn run(command: &mut Command) -> Result<String> {
    let output = command.output()?;
    ensure!(
        output.status.success(),
        "command {:?} {:?} failed ({}): {}\n{}",
        command.get_program(),
        command.get_args().collect::<Vec<_>>(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[test]
fn launcher_creates_and_updates_a_thread_environment_with_the_current_cli() -> Result<()> {
    let temp = TempDir::new()?;
    let launch = temp.path().join("launch");
    let bin = temp.path().join("bin");
    let agent_cli = temp.path().join("agent cli workspace");
    fs::create_dir(&launch)?;
    fs::create_dir(&bin)?;
    fs::create_dir(&agent_cli)?;
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::copy(repo.join("exo.sh"), launch.join("exo.sh"))?;
    fs::create_dir(launch.join("exo"))?;
    fs::copy(repo.join("exo/SELF.md"), launch.join("exo/SELF.md"))?;
    let wrapper = bin.join("exo-test");
    executable(
        &wrapper,
        &format!(
            "#!/usr/bin/env bash\nexec {} \"$@\" --root {} --config-dir {} --home {} --secret-backend file --master-key-path {}\n",
            quote(Path::new(env!("CARGO_BIN_EXE_exo")))?,
            quote(&temp.path().join("state"))?,
            quote(&temp.path().join("profiles"))?,
            quote(&temp.path().join("home"))?,
            quote(&temp.path().join("master.key"))?,
        ),
    )?;
    // The launcher may check an image, but setup must not start a container.
    executable(
        &bin.join("docker"),
        "#!/usr/bin/env bash\n[[ \"$1 $2\" == 'image inspect' ]] || exit 99\n",
    )?;
    run(Command::new(&wrapper)
        .current_dir(&launch)
        .args([
            "vault",
            "secret",
            "create",
            "global",
            "openai",
            "--token-env",
            "TEST_MODEL_KEY",
            "--http-origin",
            "https://api.openai.com",
        ])
        .env("TEST_MODEL_KEY", "synthetic-launcher-test-key"))?;

    let env_file = temp.path().join("optional.env");
    let path = std::env::join_paths([bin.clone()].into_iter().chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))?;
    for (networking, image, mount_agent_cli) in [
        ("disabled", "test:first", false),
        ("enabled", "test:second", true),
        ("disabled", "test:third", false),
    ] {
        // Exercise the system Bash, including macOS's Bash 3.2.
        let mut command = Command::new("/bin/bash");
        command
            .arg(launch.join("exo.sh"))
            .args([
                "setup-agent",
                "--template",
                "minimal",
                "--skip-build",
                "--sandbox",
                "docker",
                "--networking",
                networking,
                "--sandbox-image",
                image,
                "--no-scheduler",
                "--no-adapters",
                "--env-file",
            ])
            .arg(&env_file)
            .arg("--module")
            .arg(repo.join("exo/harness.ts"))
            .args(["--self-repo-mount", "/workspace/exo source"])
            .current_dir(&launch)
            .env("EXO_BIN", &wrapper)
            .env("PATH", &path)
            .env_remove("EXO_AGENT_CLI_ROOT");
        if mount_agent_cli {
            command
                .arg("--agent-cli-mount")
                .arg(&agent_cli)
                .args(["--agent-cli-mount-path", "/agent cli"]);
        }
        run(&mut command)?;
        let environment: exoharness::EnvironmentDefinition = serde_json::from_str(
            &fs::read_to_string(launch.join(".exo/launch-environment.json"))?,
        )?;
        environment.validate()?;
        assert_eq!(environment.config.image, image);
        assert_eq!(
            environment.config.enable_networking,
            Some(networking == "enabled")
        );
        let mut mounts = vec![exoharness::FileSystemMount {
            host_path: fs::canonicalize(&launch)?.display().to_string(),
            mount_path: "/workspace/exo source".to_string(),
            mode: exoharness::FileSystemMountMode::ReadWrite,
            internal: None,
        }];
        if mount_agent_cli {
            mounts.push(exoharness::FileSystemMount {
                host_path: fs::canonicalize(&agent_cli)?.display().to_string(),
                mount_path: "/agent cli".to_string(),
                mode: exoharness::FileSystemMountMode::ReadWrite,
                internal: None,
            });
        }
        assert_eq!(
            environment.config.file_system_mounts.as_ref(),
            Some(&mounts)
        );
        // A normal resume must retain the saved mounts without launch overrides.
        run(Command::new(&wrapper).current_dir(&launch).args([
            "agent",
            "run",
            "--agent",
            "exo-agent",
            "--thread",
            "dev",
        ]))?;
        let state = exoharness::BasicExoHarnessConfig {
            root: temp.path().join("state/exoharness"),
            secret_backend: exoharness::SecretBackendChoice::File {
                path: Some(temp.path().join("master.key")),
            },
            sandbox_default: exoharness::SandboxProvider::Docker,
            sandbox_policy: None,
            sandbox_backends: vec![exoharness::SandboxBackendRegistration::docker()],
        };
        tokio::runtime::Runtime::new()?.block_on(async {
            let harness = exoharness::BasicExoHarness::new(state).await?;
            let agent = exo_managed_agents::find_agent(&harness, "exo-agent").await?;
            let thread = exo_managed_agents::find_thread(agent.as_ref(), "dev").await?;
            assert_eq!(thread.record().environment.as_ref(), Some(&environment));
            let config = executor::load_conversation_config(thread.as_ref()).await?;
            assert_eq!(config.mounts, mounts);
            assert_eq!(
                config.sandbox_scope,
                Some(executor::SandboxScope::Conversation)
            );
            Ok::<_, anyhow::Error>(())
        })?;
        // Exercise both absent and present optional env files on the next launch.
        fs::write(&env_file, "UNUSED_TEST_SETTING=present\n")?;
    }
    exo_managed_agents::AgentDefinition::parse(fs::read_to_string(
        launch.join(".exo/launch-agent.md"),
    )?)?;
    Ok(())
}
