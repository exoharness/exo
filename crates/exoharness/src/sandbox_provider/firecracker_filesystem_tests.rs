use super::*;

fn fixture(config: &FirecrackerConfig) -> Result<SnapshotPayload> {
    let manifest = FilesystemManifest {
        template: false,
        key: "a".repeat(64),
        sandbox_id: "filesystem-test".into(),
        base_image_sha256: "b".repeat(64),
        layout: FilesystemLayout {
            workdir: "/workspace".into(),
            workspaces: vec![],
            mounts: vec![],
        },
        disk_sizes: vec![4],
    };
    let directory = manifest.directory(config);
    fs::create_dir_all(&directory)?;
    File::create(directory.join("lease"))?;
    fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec(&manifest)?,
    )?;
    for name in ["base.ext4", "overlay.ext4"] {
        fs::write(directory.join(name), b"disk")?;
        fs::set_permissions(directory.join(name), Permissions::from_mode(0o444))?;
    }
    manifest.payload()
}

#[test]
fn duplicate_restore_destinations_are_rejected() -> Result<()> {
    let root = tempfile::tempdir()?;
    let target = root.path().join("resource.ext4");
    fs::write(&target, b"existing disk")?;
    let targets = [
        (root.path().join("resource-0.ext4"), Some(target.clone())),
        (root.path().join("resource-1.ext4"), Some(target.clone())),
    ];
    let error = lock_restore_destinations(&targets)
        .err()
        .context("duplicate destinations must fail")?;
    assert!(
        error
            .to_string()
            .contains("duplicate filesystem restore destination")
    );
    assert_eq!(fs::read(&target)?, b"existing disk");
    assert!(!target.with_extension("lock").exists());
    Ok(())
}

#[test]
fn required_disks_are_validated_and_upload_lease_prevents_deletion() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let config = FirecrackerConfig {
        state_root: temp.path().into(),
        ..Default::default()
    };
    let payload = fixture(&config)?;
    let capture = open_capture(&config, payload.clone())?;
    assert!(delete_capture(&config, payload.clone()).is_err());
    assert!(capture.disks[0].exists());
    let disk = capture.disks[0].clone();
    drop(capture);
    fs::set_permissions(&disk, Permissions::from_mode(0o644))?;
    assert!(open_capture(&config, payload.clone()).is_err());
    fs::write(&disk, b"truncated")?;
    fs::set_permissions(&disk, Permissions::from_mode(0o444))?;
    assert!(open_capture(&config, payload.clone()).is_err());
    fs::remove_file(&disk)?;
    assert!(open_capture(&config, payload.clone()).is_err());
    delete_capture(&config, payload.clone())?;
    delete_capture(&config, payload)?;
    Ok(())
}

#[tokio::test]
async fn restored_base_stays_resolvable_after_capture_deletion_and_is_deduplicated() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let config = FirecrackerConfig {
        state_root: temp.path().into(),
        ..Default::default()
    };
    let payload = fixture(&config)?;
    let capture = open_capture(&config, payload.clone())?;
    fs::set_permissions(&capture.base_image, Permissions::from_mode(0o644))?;
    let mut image = vec![0; 4096];
    image[1080..1082].copy_from_slice(&[0x53, 0xef]);
    fs::write(&capture.base_image, image)?;
    fs::set_permissions(&capture.base_image, Permissions::from_mode(0o444))?;
    let digest =
        crate::sandbox_provider::firecracker_image::sha256_hex_of_file(&capture.base_image)?;
    let cached = cache_base_image(&config, &capture.base_image, &digest)?;
    let duplicate = temp.path().join("another-base.ext4");
    fs::copy(&cached, &duplicate)?;
    assert_eq!(cache_base_image(&config, &duplicate, &digest)?, cached);
    assert_eq!(
        fs::metadata(&cached)?.ino(),
        fs::metadata(&capture.base_image)?.ino()
    );
    assert!(delete_capture(&config, payload.clone()).is_err());
    drop(capture);
    delete_capture(&config, payload)?;
    assert_eq!(
        crate::sandbox_provider::firecracker_image::resolve_image(
            &config.state_root,
            cached.to_str().context("image path is not UTF-8")?,
            1,
            &[],
            &[],
        )
        .await?,
        cached
    );
    Ok(())
}

#[test]
fn imported_base_cache_hit_does_not_read_the_supplied_image() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = FirecrackerConfig {
        state_root: root.path().into(),
        ..Default::default()
    };
    let source = root.path().join("source.ext4");
    fs::write(&source, b"immutable base")?;
    fs::set_permissions(&source, Permissions::from_mode(0o444))?;
    let digest = crate::sandbox_provider::firecracker_image::sha256_hex_of_file(&source)?;
    let cached = cache_base_image(&config, &source, &digest)?;
    fs::remove_file(&source)?;
    let imported = root.path().join("imported.ext4");
    prepare_imported_base(&config, &source, &digest, &imported, true)?;
    assert_eq!(fs::metadata(&cached)?.ino(), fs::metadata(&imported)?.ino());
    Ok(())
}

#[test]
fn restored_disks_leave_previous_contents_until_commit() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let staged = temp.path().join("staged.ext4");
    let target = temp.path().join("workspace.ext4");
    fs::write(&staged, b"captured")?;
    fs::write(&target, b"previous")?;
    let install = || InstalledDisks {
        targets: vec![(staged.clone(), target.clone())],
        _locks: vec![],
    };
    drop(install());
    assert_eq!(fs::read(&target)?, b"previous");
    install().commit()?;
    assert_eq!(fs::read(&target)?, b"captured");
    assert_eq!(fs::metadata(&target)?.ino(), fs::metadata(&staged)?.ino());
    Ok(())
}

#[test]
fn malformed_references_fail_before_accessing_host_paths() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let config = FirecrackerConfig {
        state_root: temp.path().into(),
        ..Default::default()
    };
    let mut manifest = FilesystemManifest::from_payload(&fixture(&config)?)?;
    manifest.key = "../../outside".into();
    assert!(FilesystemManifest::from_payload(&manifest.payload()?).is_err());
    manifest.key = "a".repeat(64);
    manifest.disk_sizes.clear();
    assert!(FilesystemManifest::from_payload(&manifest.payload()?).is_err());
    Ok(())
}

#[cfg(target_os = "linux")]
fn native_request(config: &FirecrackerConfig, id: &str, image: &str) -> Result<SandboxRequest> {
    let agent_id = crate::Uuid7::now();
    let thread_id = crate::Uuid7::now();
    let directory = config
        .state_root
        .join("resources/threads")
        .join(agent_id.to_string())
        .join(thread_id.to_string())
        .join("repo");
    fs::create_dir_all(&directory)?;
    let volume = directory.join("volume.ext4");
    File::create(&volume)?.set_len(1024 * 1024 * 1024)?;
    run_checked("mkfs.ext4", &["-q", "-F", &volume.to_string_lossy()])?;
    for field in ["uid", "gid"] {
        run_checked(
            "debugfs",
            &[
                "-w",
                "-R",
                &format!("set_inode_field / {field} 10001"),
                &volume.to_string_lossy(),
            ],
        )?;
    }
    Ok(SandboxRequest {
        sandbox_id: id.into(),
        scope: crate::ResourceScope::Thread {
            agent_id,
            thread_id,
        },
        spec: SandboxSpec {
            image: image.into(),
            resources: Some(
                SandboxResourceShape::new(1, 1024).context("invalid test resource shape")?,
            ),
            default_workdir: "/workspace".into(),
            mounts: vec![crate::SandboxMount {
                host_path: directory,
                guest_path: "/repo".into(),
                access: crate::SandboxMountAccess::ReadWrite,
                internal: true,
            }],
            durable_file_systems: vec![crate::DurableFileSystem {
                name: "workspace".into(),
                mount_path: "/workspace".into(),
                mode: FileSystemMountMode::ReadWrite,
            }],
            policy: SandboxNetworkPolicy::Disabled.into(),
            tcp_ports: vec![],
        },
        lifecycle: crate::SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(600)),
        },
        provider_state: None,
    })
}

#[cfg(target_os = "linux")]
async fn shell(handle: &Arc<dyn ManagedSandboxHandle>, script: &str) -> Result<String> {
    let output = handle
        .exec(&SandboxCommand {
            argv: vec!["/bin/sh".into(), "-ec".into(), script.into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: Some("/".into()),
            timeout: Some(Duration::from_secs(10)),
        })
        .await?;
    ensure!(
        output.ok,
        "guest command failed: {}{}",
        output.stdout,
        output.stderr
    );
    Ok(output.stdout)
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires root, KVM, FIRECRACKER_IMAGE, and TMPDIR on XFS"]
async fn filesystem_snapshot_suspend_restart_and_import_round_trip() -> Result<()> {
    let temp = tempfile::Builder::new().prefix("exo-fs-").tempdir()?.keep();
    eprintln!("native test state: {}", temp.display());
    let image = std::env::var("FIRECRACKER_IMAGE")?;
    let mut config = FirecrackerConfig {
        state_root: temp.join("a"),
        allowed_local_images: vec![PathBuf::from(&image)],
        image_size_gib: 1,
        workspace_size_gib: 1,
        max_machines: NonZeroUsize::new(1),
        ..Default::default()
    };
    if let Some(initramfs) = std::env::var_os("EXO_FIRECRACKER_INITRAMFS") {
        config.initramfs = PathBuf::from(initramfs);
    }
    if let Some(binary) = std::env::var_os("EXO_FIRECRACKER_BINARY") {
        config.firecracker_bin = binary.into();
    }
    if let Some(binary) = std::env::var_os("EXO_FIRECRACKER_JAILER") {
        config.jailer_bin = binary.into();
    }
    let backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    let mut request = native_request(&config, "retained", &image)?;
    let handle = backend.acquire(request.clone()).await?;
    request.provider_state = handle.provider_state();
    shell(&handle, "printf '#!/bin/sh\nprintf root-installed:\ncat /repo/edit /workspace/state\n' > /tmp/retained-tool; chmod +x /tmp/retained-tool; printf captured > /repo/edit; printf workspace > /workspace/state").await?;
    assert!(handle.snapshot(crate::SnapshotKind::Full).await.is_err());
    let snapshot = handle.snapshot(crate::SnapshotKind::Filesystem).await?;
    shell(&handle, "printf mutated > /repo/edit")
        .await
        .context("mutating after capture")?;
    assert!(
        backend
            .acquire_from_snapshot(request.clone(), snapshot.clone())
            .await
            .is_err()
    );
    assert_eq!(
        shell(&handle, "/tmp/retained-tool")
            .await
            .context("reading after rejected restore")?,
        "root-installed:mutatedworkspace"
    );
    let suspended = backend
        .suspend(request.clone(), crate::SnapshotKind::Filesystem)
        .await?;
    assert_eq!(handle.is_running().await?, Some(false));
    drop(handle);
    drop(backend);

    let backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    let restored = backend
        .acquire_from_snapshot(request.clone(), suspended.clone())
        .await?;
    assert_eq!(
        shell(&restored, "/tmp/retained-tool").await?,
        "root-installed:mutatedworkspace"
    );
    request.spec.image = restored
        .effective_image()
        .context("restored image is missing")?;
    request.provider_state = restored.provider_state();
    backend.delete_snapshot(suspended.clone()).await?;
    let reacquired = backend.acquire(request.clone()).await?;
    assert_eq!(
        shell(&reacquired, "/tmp/retained-tool").await?,
        "root-installed:mutatedworkspace"
    );
    drop(reacquired);
    backend.terminate(request.clone()).await?;
    drop(restored);
    let restored = backend
        .acquire_from_snapshot(request.clone(), snapshot.clone())
        .await?;
    assert_eq!(
        shell(&restored, "/tmp/retained-tool").await?,
        "root-installed:capturedworkspace"
    );
    backend.terminate(request.clone()).await?;
    drop(restored);

    let files = backend.filesystem_snapshot_files(snapshot.clone()).await?;
    let config_b = FirecrackerConfig {
        state_root: temp.join("b"),
        ..config.clone()
    };
    let worker_b = FirecrackerSandboxBackend::new(config_b.clone()).await?;
    let mismatched = config_b.state_root.join("mismatched-base.ext4");
    let wrong_digest = "0".repeat(64);
    let error = prepare_imported_base(
        &config_b,
        &files.base_image,
        &wrong_digest,
        &mismatched,
        true,
    )
    .unwrap_err();
    assert!(error.to_string().contains("base image does not match"));
    assert!(!base_image_cache_path(&config_b, &wrong_digest).exists());
    fs::remove_file(mismatched)?;
    let imported = worker_b
        .import_filesystem_snapshot(
            snapshot.clone(),
            files.base_image.clone(),
            files.disks.clone(),
        )
        .await?;
    let target = native_request(&config_b, "imported", &image)?;
    let mut wrong_layout = target.clone();
    wrong_layout.spec.mounts[0].guest_path = "/wrong".into();
    assert!(
        worker_b
            .acquire_from_snapshot(wrong_layout, imported.clone())
            .await
            .is_err()
    );
    let restored = worker_b
        .acquire_from_snapshot(target.clone(), imported.clone())
        .await?;
    assert_eq!(
        shell(&restored, "/tmp/retained-tool").await?,
        "root-installed:capturedworkspace"
    );
    worker_b.terminate(target).await?;
    drop(restored);
    worker_b.delete_snapshot(imported).await?;
    drop(worker_b);
    drop(files);
    backend.delete_snapshot(snapshot).await?;
    backend.delete_snapshot(suspended).await?;
    let mut seed = native_request(&config, "seed", &image)?;
    seed.spec.mounts.clear();
    seed.spec.durable_file_systems.clear();
    let source = backend.acquire(seed.clone()).await?;
    shell(&source, "printf prebuilt > /tmp/prebuild-marker").await?;
    let seed_capture = source
        .snapshot_template(crate::SnapshotKind::Filesystem)
        .await?;
    backend.terminate(seed.clone()).await?;
    drop(source);
    let ca = rcgen::generate_simple_self_signed(vec!["proxy.test".into()])?;
    let placeholder = "exo_egress_0123456789abcdef0123456789abcdef";
    let proxied = backend.with_external_proxy(crate::egress::ExternalProxyConfig {
        url: url::Url::parse("http://127.0.0.1:9401")?,
        username: "filesystem-test".into(),
        password: "filesystem-proxy-authorization".into(),
        ca_pem: ca.cert.pem(),
        environment: HashMap::from([("TEST_TOKEN".into(), placeholder.into())]),
    })?;
    for id in ["seed-first", "seed-second"] {
        let mut target = native_request(&config, id, &image)?;
        target.spec.policy = crate::EgressPolicy {
            networking: SandboxNetworkPolicy::Limited {
                allowed_hosts: vec!["github.com".into()],
            },
            allowed_tcp_ports: Some(vec![443]),
            credentials: vec![crate::EgressCredentialBinding {
                name: "test-token".into(),
                environment_variable: "TEST_TOKEN".into(),
                networking: crate::CredentialNetworkPolicy::Limited {
                    allowed_hosts: vec!["github.com".into()],
                },
                injection_location: crate::CredentialInjectionLocation { header: true },
            }],
        };
        let restored = tokio::time::timeout(
            Duration::from_secs(30),
            proxied.acquire_from_snapshot(target.clone(), seed_capture.clone()),
        )
        .await
        .context("protected filesystem template restore timed out")??;
        assert_eq!(
            restored.command_environment().await?["TEST_TOKEN"],
            placeholder
        );
        assert_eq!(
            shell(&restored, "cat /tmp/prebuild-marker").await?,
            "prebuilt"
        );
        shell(
            &restored,
            "printf private > /repo/edit; printf workspace > /workspace/state",
        )
        .await?;
        let retained = proxied
            .suspend(target.clone(), crate::SnapshotKind::Filesystem)
            .await?;
        drop(restored);
        let resumed = tokio::time::timeout(
            Duration::from_secs(30),
            proxied.acquire_from_snapshot(target.clone(), retained.clone()),
        )
        .await
        .context("protected filesystem resume timed out")??;
        assert_eq!(
            resumed.command_environment().await?["TEST_TOKEN"],
            placeholder
        );
        assert_eq!(shell(&resumed, "cat /repo/edit").await?, "private");
        proxied.terminate(target).await?;
        drop(resumed);
        backend.delete_snapshot(retained).await?;
    }
    backend.delete_snapshot(seed_capture).await?;
    drop(backend);
    fs::remove_dir_all(temp)?;
    Ok(())
}

#[test]
fn capture_rejects_runtimes_with_the_vsock_resume_bug() {
    assert!(validate_capture_runtime("v1.16.1").is_err());
    assert!(validate_capture_runtime("v1.16.2").is_ok());
    assert!(validate_capture_runtime("v1.17.0").is_ok());
}

#[test]
fn collection_preserves_cataloged_captures_and_checks_manifest_paths() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let config = FirecrackerConfig {
        state_root: temp.path().into(),
        ..Default::default()
    };
    let kept = fixture(&config)?;
    let mut orphan = FilesystemManifest::from_payload(&kept)?;
    orphan.sandbox_id = "another-sandbox".into();
    let target = orphan.directory(&config);
    fs::create_dir_all(&target)?;
    fs::write(target.join("manifest.json"), serde_json::to_vec(&orphan)?)?;
    let mismatched = target.parent().unwrap().join("mismatched");
    fs::create_dir(&mismatched)?;
    fs::write(
        mismatched.join("manifest.json"),
        serde_json::to_vec(&orphan)?,
    )?;
    assert_eq!(collect_captures(&config, vec![kept.clone()])?, 1);
    assert!(!target.exists());
    assert!(mismatched.exists());
    assert!(open_capture(&config, kept).is_ok());
    Ok(())
}

#[test]
fn filesystem_and_memory_captures_share_a_budget_without_charging_external_base_links() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let config = FirecrackerConfig {
        state_root: temp.path().into(),
        ..Default::default()
    };
    let payload = fixture(&config)?;
    let mut manifest = FilesystemManifest::from_payload(&payload)?;
    let first = manifest.directory(&config);
    let cache = temp.path().join("images/rootfs.ext4");
    fs::create_dir_all(cache.parent().unwrap())?;
    fs::write(&cache, [7; 4096])?;
    File::open(&cache)?.sync_all()?;
    fs::remove_file(first.join("base.ext4"))?;
    fs::hard_link(&cache, first.join("base.ext4"))?;
    manifest.key = "d".repeat(64);
    let second = manifest.directory(&config);
    fs::create_dir_all(&second)?;
    fs::hard_link(&cache, second.join("base.ext4"))?;
    let snapshots = temp.path().join("snapshots");
    fs::create_dir(&snapshots)?;
    fs::write(snapshots.join("memory"), [9; 4096])?;
    File::open(snapshots.join("memory"))?.sync_all()?;
    let base_bytes = fs::metadata(&cache)?.blocks() * 512;
    let pool_bytes = snapshot_directory_bytes(
        &config.state_root.join("filesystem-snapshots"),
        &mut HashSet::new(),
    )?;
    let memory_bytes = snapshot_directory_bytes(&snapshots, &mut HashSet::new())?;
    assert!(base_bytes > 0 && memory_bytes > 0);
    assert_eq!(
        retained_snapshot_bytes(&config)?,
        pool_bytes + memory_bytes - base_bytes
    );
    fs::remove_file(cache)?;
    assert_eq!(retained_snapshot_bytes(&config)?, pool_bytes + memory_bytes);
    Ok(())
}
