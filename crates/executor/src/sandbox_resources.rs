//! Git workspaces for backends without host filesystem mounts.
use crate::{AgentConfig, ConversationConfig};
use anyhow::{Result, ensure};
use exoharness::resources::{PreparedResource, ResourceSource};
use exoharness::{ConversationHandle, RunInSandboxRequest};
use futures::io::{AsyncBufReadExt, BufReader};

pub(crate) async fn materialize(
    thread: &dyn ConversationHandle,
    agent: &AgentConfig,
    config: &ConversationConfig,
    progress: &(dyn Fn(String) + Send + Sync),
) -> Result<()> {
    let id = crate::conversation_sandbox::ensure_conversation_sandbox(thread, agent, config, None)
        .await?;
    let _activity = thread.sandbox_activity(id.clone()).await?;
    for resource in &config.resources {
        let ResourceSource::GitRepository { url: Some(url), .. } = &resource.definition.source
        else {
            anyhow::bail!("sandbox resources require a Git URL");
        };
        let url = url::Url::parse(url)?;
        let repository = url[url::Position::BeforeHost..url::Position::AfterPath]
            .trim_end_matches('/')
            .trim_end_matches(".git");
        progress(format!("Preparing Git resource {repository}"));
        let process = thread
            .run_in_sandbox(RunInSandboxRequest {
                id: id.clone(),
                command: vec!["/bin/sh".into(), "-ec".into(), git_script(resource)?],
                env: Default::default(),
            })
            .await?;
        let mut parts = process.into_parts();
        futures::io::AsyncWriteExt::close(&mut parts.stdin).await?;
        let mut stdout = futures::io::sink();
        let mut stderr = Vec::new();
        let (out, err, exit) = futures::join!(
            futures::io::copy(&mut parts.stdout, &mut stdout),
            async {
                let mut reader = BufReader::new(parts.stderr);
                let mut record = Vec::new();
                let mut last = None;
                while reader.read_until(b'\r', &mut record).await? > 0 {
                    stderr.extend_from_slice(&record);
                    for line in record.split(|byte| matches!(byte, b'\r' | b'\n')) {
                        if let Some(update) = exoharness::resources::git_progress(line)
                            && last != Some(update)
                        {
                            let (phase, percent) = update;
                            progress(format!("{phase} {repository}: {percent}%"));
                            last = Some(update);
                        }
                    }
                    record.clear();
                }
                Ok::<_, std::io::Error>(())
            },
            parts.wait
        );
        out?;
        err?;
        ensure!(
            exit? == 0,
            "preparing Git resource {} failed: {}",
            resource.definition.name,
            String::from_utf8_lossy(&stderr)
        );
    }
    Ok(())
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn git_script(resource: &PreparedResource) -> Result<String> {
    let definition = &resource.definition;
    let ResourceSource::GitRepository {
        url: Some(url),
        checkout,
        credential,
        ..
    } = &definition.source
    else {
        anyhow::bail!("sandbox resources require a Git URL");
    };
    let mut identity = resource.clone();
    if let ResourceSource::GitRepository { credential, .. } = &mut identity.definition.source {
        *credential = None;
    }
    identity.definition.mount_path = Some(definition.mount_path().into_owned());
    let identity = quote(&serde_json::to_string(&identity)?);
    let destination = quote(&definition.mount_path());
    // The marker is captured with the sandbox, so restoring never fetches or
    // resets a workspace containing the agent's changes. A different resource
    // definition requires a new thread rather than overwriting that workspace.
    let mut script = format!(
        r#"set -eu
export GIT_TERMINAL_PROMPT=0
destination={destination}
marker="$destination/.git/exo-resource.json"
if [ -f "$marker" ]; then
  [ "$(cat "$marker")" = {identity} ] && [ -d "$destination/.git" ] || {{ echo 'Git resource changed or workspace is missing; start a new thread' >&2; exit 1; }}
  exit 0
fi
[ ! -L "$destination" ] || {{ echo 'Git resource path is a symlink' >&2; exit 1; }}
if [ -d "$destination" ]; then
  [ -z "$(find "$destination" -mindepth 1 -maxdepth 1 -print -quit)" ] || {{ echo 'Git resource path is not empty' >&2; exit 1; }}
fi
mkdir -p "$(dirname "$destination")"
staging=$(mktemp -d "$(dirname "$destination")/.exo-git-XXXXXX")
trap 'rm -rf "$staging"' EXIT
"#
    );
    let auth = if credential.is_some() {
        format!(
            " -c credential.helper= -c {} -c {}",
            quote(&format!(
                "credential.{url}.helper={}",
                definition.git_credential_helper()
            )),
            quote(&format!("credential.{url}.useHttpPath=true"))
        )
    } else {
        String::new()
    };
    let commit = checkout.as_deref().filter(|reference| {
        matches!(reference.len(), 40 | 64) && reference.bytes().all(|c| c.is_ascii_hexdigit())
    });
    let branch = checkout
        .as_deref()
        .filter(|_| commit.is_none())
        .map(|reference| format!(" --branch {}", quote(reference)))
        .unwrap_or_default();
    if let Some(sha) = commit {
        script.push_str(&format!("git init \"$staging/repo\"\ngit -C \"$staging/repo\" remote add origin {}\ngit{auth} -C \"$staging/repo\" fetch --progress --depth=1 --no-tags --no-recurse-submodules origin {}\ngit -C \"$staging/repo\" checkout --detach FETCH_HEAD\n", quote(url), quote(sha)));
    } else {
        script.push_str(&format!("git{auth} clone --progress --depth=1 --single-branch --no-tags --no-recurse-submodules{branch} -- {} \"$staging/repo\"\n", quote(url)));
    }
    if credential.is_some() {
        script.push_str(&format!(
            "git -C \"$staging/repo\" config {} {}\ngit -C \"$staging/repo\" config {} true\n",
            quote(&format!("credential.{url}.helper")),
            quote(&definition.git_credential_helper()),
            quote(&format!("credential.{url}.useHttpPath"))
        ));
    }
    script.push_str(&format!(
        r#"if [ -d "$destination" ]; then rmdir "$destination"; fi
printf '%s' {identity} > "$staging/repo/.git/exo-resource.json"
mv "$staging/repo" "$destination"
"#
    ));
    Ok(script)
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use exoharness::resources::ResourceDefinition;
    use std::{fs, process::Command};

    #[test]
    fn git_workspace_preparation_preserves_edits_and_retries_failed_clones() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source");
        fs::create_dir(&source)?;
        let git = |args: &[&str]| -> Result<String> {
            let result = Command::new("git")
                .current_dir(&source)
                .args(args)
                .output()?;
            ensure!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            Ok(String::from_utf8(result.stdout)?.trim().into())
        };
        git(&["init", "--initial-branch=main"])?;
        fs::write(source.join("README"), "original")?;
        git(&["add", "."])?;
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "initial",
        ])?;
        let sha = git(&["rev-parse", "HEAD"])?;
        fs::write(source.join("README"), "newer")?;
        git(&["add", "."])?;
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "second",
        ])?;
        let mut resource = PreparedResource {
            snapshot: None,
            definition: ResourceDefinition {
                name: "repo".into(),
                mount_path: Some(directory.path().join("workspace").to_string_lossy().into()),
                mode: exoharness::FileSystemMountMode::ReadWrite,
                source: ResourceSource::GitRepository {
                    path: None,
                    url: Some("https://git.example/repo".into()),
                    credential: Some("git".into()),
                    checkout: Some("missing".into()),
                },
            },
        };
        let run = |resource: &PreparedResource| -> Result<std::process::Output> {
            Ok(Command::new("/bin/sh")
                .args(["-ec", &git_script(resource)?])
                .env("GIT_CONFIG_COUNT", "1")
                .env(
                    "GIT_CONFIG_KEY_0",
                    format!("url.file://{}.insteadOf", source.display()),
                )
                .env("GIT_CONFIG_VALUE_0", "https://git.example/repo")
                .output()?)
        };
        assert!(!run(&resource)?.status.success());
        if let ResourceSource::GitRepository { checkout, .. } = &mut resource.definition.source {
            *checkout = Some(sha);
        }
        let output = run(&resource)?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let readme = std::path::Path::new(resource.definition.mount_path().as_ref()).join("README");
        assert_eq!(fs::read_to_string(&readme)?, "original");
        let cloned_git = |args: &[&str]| -> Result<String> {
            let result = Command::new("git")
                .current_dir(resource.definition.mount_path().as_ref())
                .args(args)
                .output()?;
            ensure!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            Ok(String::from_utf8(result.stdout)?.trim().into())
        };
        assert_eq!(
            cloned_git(&["rev-parse", "--is-shallow-repository"])?,
            "true"
        );
        assert_eq!(
            cloned_git(&["remote", "get-url", "origin"])?,
            "https://git.example/repo"
        );
        assert_eq!(cloned_git(&["rev-list", "--count", "HEAD"])?, "1");
        fs::write(&readme, "agent changes")?;
        assert!(run(&resource)?.status.success());
        assert_eq!(fs::read_to_string(&readme)?, "agent changes");
        if let ResourceSource::GitRepository { checkout, .. } = &mut resource.definition.source {
            *checkout = Some("main".into());
        }
        assert!(!run(&resource)?.status.success());
        assert_eq!(fs::read_to_string(readme)?, "agent changes");
        Ok(())
    }
}
