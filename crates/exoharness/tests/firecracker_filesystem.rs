#![cfg(feature = "firecracker")]

#[tokio::test]
#[ignore = "requires root, KVM, TMPDIR on XFS, and the Firecracker host artifact bundle"]
async fn firecracker_filesystem_capture_restores_overlay_and_workspace() -> anyhow::Result<()> {
    use exoharness::{
        DurableFileSystem, FileSystemMountMode, FirecrackerConfig, FirecrackerSandboxBackend,
        ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxLifecycleConfig,
        SandboxNetworkPolicy, SandboxRequest, SandboxResourceShape, SandboxSpec, SnapshotKind,
    };
    use std::sync::Arc;
    use std::time::Duration;

    async fn shell(handle: &Arc<dyn ManagedSandboxHandle>, script: &str) -> anyhow::Result<String> {
        let output = handle
            .exec(&SandboxCommand {
                argv: vec!["/bin/sh".into(), "-ec".into(), script.into()],
                timeout: Some(Duration::from_secs(10)),
                env: Default::default(),
                cwd: None,
                display_argv: None,
            })
            .await?;
        anyhow::ensure!(output.ok, "guest command failed: {output:?}");
        Ok(output.stdout)
    }

    let root = tempfile::TempDir::new()?;
    let backend = FirecrackerSandboxBackend::new(FirecrackerConfig {
        state_root: root.path().join("state"),
        firecracker_bin: std::env::var("EXO_FIRECRACKER_BINARY")?.into(),
        jailer_bin: std::env::var("EXO_FIRECRACKER_JAILER")?.into(),
        image_size_gib: 1,
        workspace_size_gib: 1,
        allow_guest_root: false,
        allowed_registries: vec!["docker.io".into()],
        ..Default::default()
    })
    .await?;
    let source = SandboxRequest {
        sandbox_id: "filesystem-source".into(),
        scope: exoharness::ResourceScope::Global,
        provider_state: None,
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        spec: SandboxSpec {
            image: "docker.io/library/alpine:3.22".into(),
            resources: SandboxResourceShape::new(1, 512),
            default_workdir: "/workspace".into(),
            policy: SandboxNetworkPolicy::Disabled.into(),
            tcp_ports: vec![],
            mounts: vec![],
            durable_file_systems: vec![DurableFileSystem {
                name: "workspace".into(),
                mount_path: "/workspace".into(),
                mode: FileSystemMountMode::ReadWrite,
            }],
        },
    };
    let mut restored = source.clone();
    restored.sandbox_id = "filesystem-restored".into();
    let result = async {
        let handle = backend.acquire(source.clone()).await?;
        shell(
            &handle,
            "printf overlay > /tmp/exo-capture; printf workspace > /workspace/exo-capture",
        )
        .await?;
        let snapshot = handle.snapshot(SnapshotKind::Filesystem).await?;
        let handle = backend
            .acquire_from_snapshot(restored.clone(), snapshot.clone())
            .await?;
        let contents = shell(&handle, "cat /tmp/exo-capture /workspace/exo-capture").await?;
        anyhow::ensure!(
            contents == "overlayworkspace",
            "restored filesystem contents were {contents:?}"
        );
        // The restored VM's workspace is now linked to a different host disk,
        // and its base image comes from the capture, so capture it again.
        let recaptured = handle.snapshot(SnapshotKind::Filesystem).await?;
        backend.delete_snapshot(recaptured).await?;
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
