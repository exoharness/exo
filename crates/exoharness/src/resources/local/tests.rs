use super::*;
use crate::{FileSystemMountMode, Uuid7};

fn resource(source: ResourceSource) -> ResourceDefinition {
    ResourceDefinition {
        name: "code".into(),
        mount_path: "/workspace".into(),
        mode: FileSystemMountMode::ReadWrite,
        source,
    }
}

fn commit(path: &Path, contents: &str) -> Result<String> {
    fs::write(path.join("README"), contents)?;
    git(path, ["add", "README"], None)?;
    git(
        path,
        [
            "-c",
            "user.name=Exo test",
            "-c",
            "user.email=exo@example.invalid",
            "commit",
            "-m",
            contents,
        ],
        None,
    )?;
    Ok(git(path, ["rev-parse", "HEAD"], None)?.trim().into())
}

#[test]
fn directory_copy_preserves_links_and_excludes_runtime_state() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir_all(source.join(".exo"))?;
    fs::write(source.join("file"), "contents")?;
    fs::write(source.join(".exo/secret"), "never copied")?;
    #[cfg(unix)]
    std::os::unix::fs::symlink("file", source.join("link"))?;
    let target = temp.path().join("target");
    copy_directory(&source, &target, &source.join(".exo"), false)?;
    assert!(!target.join(".exo").exists());
    assert_eq!(fs::read_to_string(target.join("file"))?, "contents");
    #[cfg(unix)]
    assert_eq!(fs::read_link(target.join("link"))?, Path::new("file"));
    Ok(())
}

#[test]
fn imported_git_is_independent_and_preserves_dirty_index() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    git(&source, ["init", "--initial-branch=main"], None)?;
    commit(&source, "committed")?;
    fs::write(source.join("README"), "staged")?;
    git(&source, ["add", "README"], None)?;
    fs::write(source.join("README"), "unstaged")?;
    fs::write(source.join("untracked"), "local")?;
    let target = temp.path().join("target");
    local_git(&source, &target, &temp.path().join("state"))?;
    assert_eq!(
        git(&source, ["status", "--porcelain"], None)?,
        git(&target, ["status", "--porcelain"], None)?
    );
    git(&target, ["branch", "private"], None)?;
    assert!(git(&source, ["branch", "--list", "private"], None)?.is_empty());
    assert!(!target.join(".git/objects/info/alternates").exists());
    Ok(())
}

#[test]
#[ignore = "requires APFS disk images on macOS or reflink-capable storage on Linux"]
fn cow_resources_reuse_git_volume_and_isolate_threads() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let store = ResourceStore::new(&temp.path().join("state"))?;
    check_git_thread_isolation(&store, &temp)
}

#[cfg(all(target_os = "linux", feature = "firecracker"))]
#[test]
#[ignore = "requires root and TMPDIR on XFS with reflinks enabled"]
fn firecracker_images_reuse_git_cache_and_preserve_thread_edits() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let store = ResourceStore::image_store(&temp.path().join("state"), 1)?;
    check_git_thread_isolation(&store, &temp)
}

fn mounted_resource(store: &ResourceStore, mount: &FileSystemMount) -> Result<PathBuf> {
    let path = Path::new(&mount.host_path);
    if store.image_size_gib.is_some() {
        store.mount_volume(path)
    } else {
        Ok(path.to_owned())
    }
}

fn check_git_thread_isolation(store: &ResourceStore, temp: &tempfile::TempDir) -> Result<()> {
    store.initialize()?;
    let source = temp.path().join("origin");
    fs::create_dir(&source)?;
    git(&source, ["init", "--initial-branch=main"], None)?;
    let first_revision = commit(&source, "first")?;
    let definition = resource(ResourceSource::GitRepository {
        path: None,
        url: Some(url::Url::from_directory_path(&source).unwrap().to_string()),
        checkout: Some(GitCheckout::Branch {
            name: "main".into(),
        }),
        credential: None,
    });
    let prepared = vec![PreparedResource {
        definition,
        snapshot: None,
    }];
    let agent = Uuid7::now();
    let a = Uuid7::now();
    let b = Uuid7::now();
    let result = (|| {
        let first = store.materialize(agent, a, prepared.clone(), vec![None])?;
        let first_root = mounted_resource(store, &first[0])?;
        let first_path = first_root.as_path();
        assert_eq!(
            git(first_path, ["rev-parse", "HEAD"], None)?.trim(),
            first_revision
        );
        fs::write(first_path.join("README"), "private edit")?;
        git(first_path, ["branch", "private"], None)?;
        let second_revision = commit(&source, "second")?;
        let second = store.materialize(agent, b, prepared.clone(), vec![None])?;
        let second_root = mounted_resource(store, &second[0])?;
        let second_path = second_root.as_path();
        assert_eq!(
            git(second_path, ["rev-parse", "HEAD"], None)?.trim(),
            second_revision
        );
        assert_eq!(
            git(second_path, ["symbolic-ref", "--short", "HEAD"], None)?.trim(),
            "main"
        );
        assert!(git(second_path, ["branch", "--list", "private"], None)?.is_empty());
        assert_eq!(
            fs::read_to_string(first_path.join("README"))?,
            "private edit"
        );
        let manifests = [a, b].map(|thread| -> Result<Vec<Instance>> {
            Ok(serde_json::from_slice(&fs::read(
                store.thread_directory(agent, thread).join("resources.json"),
            )?)?)
        });
        assert_eq!(
            manifests[0].as_ref().unwrap()[0].snapshot,
            manifests[1].as_ref().unwrap()[0].snapshot
        );
        assert_eq!(fs::read_dir(store.root.join("git"))?.count(), 1);
        assert_eq!(fs::read_dir(store.root.join("snapshots"))?.count(), 0);
        store.unmount_volume(&store.thread_directory(agent, a).join("code"))?;
        fs::rename(&source, temp.path().join("offline"))?;
        let mut authenticated = prepared.clone();
        let ResourceSource::GitRepository { credential, .. } =
            &mut authenticated[0].definition.source
        else {
            unreachable!()
        };
        *credential = Some("github-git".into());
        let reopened = store.materialize(agent, a, authenticated.clone(), vec![None])?;
        let reopened_root = mounted_resource(store, &reopened[0])?;
        assert_eq!(
            git(&reopened_root, ["rev-parse", "HEAD"], None)?.trim(),
            first_revision
        );
        assert!(!git(&reopened_root, ["branch", "--list", "private"], None)?.is_empty());
        let manifest: Vec<Instance> = serde_json::from_slice(&fs::read(
            store.thread_directory(agent, a).join("resources.json"),
        )?)?;
        assert_eq!(manifest[0].prepared, authenticated[0]);
        let mut changed_source = authenticated;
        let ResourceSource::GitRepository { url, .. } = &mut changed_source[0].definition.source
        else {
            unreachable!()
        };
        *url = Some("https://github.com/other/repo".into());
        assert!(
            store
                .materialize(agent, a, changed_source, vec![None])
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(mounted_resource(store, &reopened[0])?.join("README"))?,
            "private edit"
        );
        assert!(
            store
                .materialize(agent, Uuid7::now(), prepared.clone(), vec![None])
                .is_err()
        );
        Ok(())
    })();
    let cleanup_a = store.remove_thread(agent, a);
    let cleanup_b = store.remove_thread(agent, b);
    cleanup_a?;
    cleanup_b?;
    result
}

#[test]
#[ignore = "requires APFS disk images on macOS or reflink-capable storage on Linux"]
fn local_resources_pin_prepared_snapshot_until_agent_update() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let store = ResourceStore::new(&temp.path().join("state"))?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("file"), "first")?;
    let definition = resource(ResourceSource::Directory {
        path: source.clone(),
    });
    let prepared = store.prepare(vec![definition.clone()])?;
    assert_eq!(prepared, store.prepare(vec![definition.clone()])?);
    let agent = Uuid7::now();
    let a = Uuid7::now();
    let b = Uuid7::now();
    let result = (|| {
        fs::write(source.join("file"), "second")?;
        let old = store.materialize(agent, a, prepared, vec![None])?;
        assert_eq!(
            fs::read_to_string(Path::new(&old[0].host_path).join("file"))?,
            "first"
        );
        let updated = store.prepare(vec![definition])?;
        let new = store.materialize(agent, b, updated, vec![None])?;
        assert_eq!(
            fs::read_to_string(Path::new(&new[0].host_path).join("file"))?,
            "second"
        );
        assert_eq!(
            fs::read_to_string(Path::new(&old[0].host_path).join("file"))?,
            "first"
        );
        Ok(())
    })();
    let cleanup_a = store.remove_thread(agent, a);
    let cleanup_b = store.remove_thread(agent, b);
    cleanup_a?;
    cleanup_b?;
    result
}

#[test]
#[cfg(unix)]
fn host_git_credentials_are_used_unless_a_vault_credential_is_selected() -> Result<()> {
    use std::{io::Write, os::unix::fs::PermissionsExt, process::Stdio};

    let temp = tempfile::tempdir()?;
    let helper = temp.path().join("credential-helper");
    fs::write(
        &helper,
        "#!/bin/sh\nprintf '%s\\n' username=exo password=host-test-secret\n",
    )?;
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))?;
    let config = temp.path().join("gitconfig");
    git(
        temp.path(),
        [
            OsStr::new("config"),
            OsStr::new("--file"),
            config.as_os_str(),
            OsStr::new("credential.helper"),
            helper.as_os_str(),
        ],
        None,
    )?;
    let vault = GitCredential {
        username: "x-access-token".into(),
        identity: "test".into(),
        token: "vault-test-secret".into(),
    };
    for credential in [None, Some(&vault)] {
        let (mut command, _) = git_command(temp.path(), credential);
        let mut child = command
            .env("HOME", temp.path())
            .env("GIT_CONFIG_GLOBAL", &config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ASKPASS", "/usr/bin/false")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .args(["credential", "fill"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("Git credential input")?
            .write_all(b"url=https://example.invalid/repo\n\n")?;
        let output = child.wait_with_output()?;
        let stdout = String::from_utf8(output.stdout)?;
        if credential.is_none() {
            assert!(output.status.success());
            assert!(stdout.contains("password=host-test-secret"));
        } else {
            assert!(!output.status.success());
            assert!(!stdout.contains("host-test-secret"));
        }
    }
    Ok(())
}

#[test]
fn sandbox_git_trust_is_limited_to_mounted_git_resources() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let store = ResourceStore::new(&temp.path().join("state"))?;
    let parent = temp.path().join("repos with 'quotes'");
    let repository = parent.join("* literal");
    let other = parent.join("other");
    for path in [&repository, &other] {
        fs::create_dir_all(path)?;
        git(path, ["init"], None)?;
    }
    let mut definition = resource(ResourceSource::GitRepository {
        path: Some(repository.clone()),
        url: None,
        checkout: None,
        credential: None,
    });
    definition.mount_path = repository.to_string_lossy().into_owned();
    let mut unmounted = definition.clone();
    unmounted.name = "unmounted".into();
    unmounted.mount_path = temp.path().join("unmounted").to_string_lossy().into_owned();
    let mut directory = unmounted.clone();
    directory.name = "directory".into();
    directory.mount_path = other.to_string_lossy().into_owned();
    directory.source = ResourceSource::Directory {
        path: other.clone(),
    };
    let resources: Vec<_> = [definition, unmounted, directory]
        .into_iter()
        .map(|definition| PreparedResource {
            definition,
            snapshot: None,
        })
        .collect();
    let mounts: Vec<_> = [&repository, &other]
        .into_iter()
        .map(|path| FileSystemMount {
            host_path: path.to_string_lossy().into_owned(),
            mount_path: path.to_string_lossy().into_owned(),
            mode: FileSystemMountMode::ReadWrite,
            internal: Some(true),
        })
        .collect();
    let env = HashMap::from([
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "user.name".into()),
        ("GIT_CONFIG_VALUE_0".into(), "Existing user".into()),
    ]);
    let run = |path: &Path, env: &HashMap<String, String>| -> Result<Output> {
        Ok(Command::new("git")
            .current_dir(path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .envs(env)
            .args(["status", "--porcelain"])
            .output()?)
    };
    let before = run(&repository, &env)?;
    assert!(!before.status.success());
    assert!(String::from_utf8_lossy(&before.stderr).contains("dubious ownership"));
    let agent_id = Uuid7::now();
    for external in [false, true] {
        let thread_id = Uuid7::now();
        let directory = store.thread_directory(agent_id, thread_id);
        fs::create_dir_all(&directory)?;
        if external {
            store.remember_external(agent_id, thread_id, Some(&resources))?;
        } else {
            let instances: Vec<_> = resources
                .iter()
                .cloned()
                .map(|prepared| Instance {
                    prepared,
                    snapshot: "test".into(),
                    revision: None,
                })
                .collect();
            fs::write(
                directory.join("resources.json"),
                serde_json::to_vec(&instances)?,
            )?;
        }
        let configured = store.command_env(
            ResourceScope::Thread {
                agent_id,
                thread_id,
            },
            &mounts,
            env.clone(),
        )?;
        assert_eq!(configured["GIT_CONFIG_COUNT"], "2");
        assert_eq!(configured["GIT_CONFIG_VALUE_0"], "Existing user");
        let output = run(&repository, &configured)?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!run(&other, &configured)?.status.success());
        assert_eq!(
            store.command_env(ResourceScope::Global, &mounts, env.clone())?,
            env
        );
    }
    Ok(())
}
