use super::*;

fn fixture(config: &FirecrackerConfig) -> Result<SnapshotPayload> {
    let manifest = FilesystemManifest {
        template: false,
        version: 1,
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

#[test]
fn snapshot_budget_counts_allocated_blocks_in_sparse_images() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut file = File::create(temp.path().join("disk"))?;
    file.set_len(96 * 1024 * 1024 * 1024)?;
    file.write_all(b"written extent")?;
    file.sync_all()?;
    let allocated = allocated_bytes(temp.path())?;
    assert!(allocated > 0);
    assert!(allocated < 1024 * 1024);
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

#[tokio::test]
async fn startup_collection_preserves_cataloged_captures() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let config = FirecrackerConfig {
        state_root: temp.path().into(),
        ..Default::default()
    };
    let payload = fixture(&config)?;
    let kept = FilesystemManifest::from_payload(&payload)?.key;
    let mut orphan = FilesystemManifest::from_payload(&payload)?;
    orphan.key = "c".repeat(64);
    let source = FilesystemManifest::from_payload(&payload)?.directory(&config);
    let target = orphan.directory(&config);
    fs::create_dir_all(&target)?;
    for file in ["lease", "base.ext4", "overlay.ext4"] {
        fs::copy(source.join(file), target.join(file))?;
    }
    fs::write(target.join("manifest.json"), serde_json::to_vec(&orphan)?)?;
    assert!(source.to_string_lossy().contains(&kept));
    let lease = open_capture(&config, orphan.payload()?)?;
    assert!(collect_captures(&config, vec![payload.clone()]).is_err());
    drop(lease);
    assert_eq!(collect_captures(&config, vec![payload.clone()])?, 1);
    assert_eq!(collect_captures(&config, vec![payload.clone()])?, 0);
    assert!(open_capture(&config, payload).is_ok());
    Ok(())
}
