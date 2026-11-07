#![cfg(feature = "firecracker")]

#[tokio::test]
#[ignore = "requires root, KVM, TMPDIR on XFS, and the Firecracker host artifact bundle"]
async fn firecracker_snapshot_restore_synchronizes_wall_clock() -> anyhow::Result<()> {
    use exoharness::{
        FirecrackerConfig, FirecrackerNetworkDevicePolicy, FirecrackerSandboxBackend,
        ManagedSandboxBackend, SandboxCommand, SandboxLifecycleConfig, SandboxNetworkPolicy,
        SandboxRequest, SandboxResourceShape, SandboxSpec, SnapshotKind,
    };
    use std::time::{Duration, SystemTime};

    let root = tempfile::TempDir::new()?;
    let backend = FirecrackerSandboxBackend::new(FirecrackerConfig {
        state_root: root.path().join("state"),
        firecracker_bin: std::env::var("EXO_FIRECRACKER_BINARY")?.into(),
        jailer_bin: std::env::var("EXO_FIRECRACKER_JAILER")?.into(),
        image_size_gib: 1,
        workspace_size_gib: 1,
        network_device_policy: FirecrackerNetworkDevicePolicy::AllSandboxes,
        allow_guest_root: false,
        allowed_registries: vec!["docker.io".into()],
        ..Default::default()
    })
    .await?;
    let source = SandboxRequest {
        sandbox_id: "clock-source".into(),
        scope: exoharness::ResourceScope::Global,
        provider_state: None,
        lifecycle: SandboxLifecycleConfig::default(),
        spec: SandboxSpec {
            image: "docker.io/library/alpine:3.22".into(),
            resources: SandboxResourceShape::new(1, 512),
            default_workdir: "/workspace".into(),
            policy: SandboxNetworkPolicy::Disabled.into(),
            tcp_ports: vec![],
            mounts: vec![],
            durable_file_systems: vec![],
        },
    };
    let mut restored = source.clone();
    restored.sandbox_id = "clock-restored".into();
    let result = async {
        let handle = backend.acquire(source.clone()).await?;
        let snapshot = handle.snapshot_template(SnapshotKind::Full).await?;
        backend.terminate(source.clone()).await?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let handle = backend
            .acquire_from_snapshot(restored.clone(), snapshot.clone())
            .await?;
        let before = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs();
        let output = handle
            .exec(&SandboxCommand {
                argv: vec!["/bin/date".into(), "+%s".into()],
                timeout: Some(Duration::from_secs(10)),
                env: Default::default(),
                cwd: None,
                display_argv: None,
            })
            .await?;
        let after = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs();
        anyhow::ensure!(output.ok, "reading restored clock failed: {output:?}");
        let guest: u64 = output.stdout.trim().parse()?;
        println!("restored guest time {guest}; host interval {before}..={after}");
        anyhow::ensure!(
            guest >= before.saturating_sub(1) && guest <= after + 1,
            "restored guest time {guest} outside host interval {before}..={after}"
        );
        backend.delete_snapshot(snapshot).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = backend.terminate(restored).await;
    let source_cleanup = backend.terminate(source).await;
    result?;
    cleanup?;
    source_cleanup?;
    Ok(())
}
