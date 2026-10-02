use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_exo"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("EXO_CONFIG_DIR", home.join(".config/exo"))
        .env("EXO_SECRET_BACKEND", "file")
        .env("EXO_MASTER_KEY_PATH", home.join("master-key"))
        .env("EXO_LITELLM_PRICES_PATH", home.join("prices.json"))
        .current_dir(cwd)
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[test]
fn default_local_state_is_shared_across_directories_and_root_overrides_it() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("home");
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    for path in [&home, &first, &second] {
        std::fs::create_dir(path)?;
    }
    std::fs::write(home.join("prices.json"), "{}")?;
    let file = temp.path().join("environment.yaml");
    std::fs::write(
        &file,
        "name: dev\nconfig:\n  provider: local_process\n  image: unused\n",
    )?;
    run(
        &home,
        &first,
        &[
            "environment",
            "create",
            "dev",
            "--file",
            file.to_str().unwrap(),
        ],
    )?;
    assert!(run(&home, &second, &["environment", "list"])?.contains("dev"));
    assert!(home.join(".exo/exoharness").exists());
    assert!(!first.join(".exo").exists());
    assert!(!second.join(".exo").exists());
    let isolated = temp.path().join("isolated");
    assert!(
        run(
            &home,
            &second,
            &["environment", "list", "--root", isolated.to_str().unwrap()],
        )?
        .is_empty()
    );
    assert!(run(&home, &first, &["environment", "list"])?.contains("dev"));
    Ok(())
}

#[test]
fn explicit_agent_environment_and_named_thread_work_across_directories() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("home");
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    for path in [&home, &first, &second] {
        std::fs::create_dir(path)?;
    }
    std::fs::write(home.join("prices.json"), "{}")?;
    let agent = temp.path().join("agent.md");
    std::fs::write(
        &agent,
        "---\nname: developer\nharness: basic\nconfig:\n  model: fixture\n---\nHelp with development.\n",
    )?;
    let environment = temp.path().join("environment.yaml");
    std::fs::write(
        &environment,
        "name: local-dev\nconfig:\n  provider: local_process\n  image: unused\n",
    )?;
    run(
        &home,
        &first,
        &[
            "agent",
            "create",
            "braintrust-dev",
            "--file",
            agent.to_str().context("agent path")?,
        ],
    )?;
    run(
        &home,
        &second,
        &[
            "environment",
            "create",
            "local-dev",
            "--file",
            environment.to_str().context("environment path")?,
        ],
    )?;
    let args = [
        "agent",
        "run",
        "--agent",
        "braintrust-dev",
        "--environment",
        "local-dev",
        "--thread",
        "my-project-name",
    ];
    let opened = run(&home, &first, &args)?;
    assert!(opened.contains("thread: my-project-name ("), "{opened}");
    let resumed = run(&home, &second, &args)?;
    let thread_line = |output: &str| {
        output
            .lines()
            .find(|line| line.starts_with("thread: "))
            .unwrap()
            .to_owned()
    };
    assert_eq!(thread_line(&opened), thread_line(&resumed));
    let shown = run(
        &home,
        &second,
        &["thread", "get", "braintrust-dev", "my-project-name"],
    )?;
    assert!(shown.contains("effective_sandbox_image: unused"), "{shown}");
    assert!(
        shown.contains("effective_sandbox_provider: local_process"),
        "{shown}"
    );
    assert!(!first.join(".exo").exists());
    assert!(!second.join(".exo").exists());
    Ok(())
}
