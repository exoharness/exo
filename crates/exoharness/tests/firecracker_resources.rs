#![cfg(feature = "firecracker")]

use anyhow::Context;
use exoharness::{
    FirecrackerConfig, FirecrackerNetworkDevicePolicy, FirecrackerSandboxBackend,
    ManagedSandboxBackend, SandboxCommand, SandboxLifecycleConfig, SandboxNetworkPolicy,
    SandboxRequest, SandboxResourceShape, SandboxSpec, SnapshotKind,
};
use futures::io::{AsyncReadExt, AsyncWriteExt};
use std::time::Duration;

fn command(argv: &[&str]) -> SandboxCommand {
    SandboxCommand {
        argv: argv.iter().map(|arg| (*arg).into()).collect(),
        timeout: Some(Duration::from_secs(60)),
        env: Default::default(),
        cwd: None,
        display_argv: None,
    }
}

#[tokio::test]
#[ignore = "requires root, KVM, TMPDIR on XFS, and the Firecracker host artifact bundle"]
async fn firecracker_memory_growth_retries_and_restores() -> anyhow::Result<()> {
    let root = tempfile::TempDir::new()?;
    let backend = FirecrackerSandboxBackend::new(FirecrackerConfig {
        state_root: root.path().join("state"),
        firecracker_bin: std::env::var("EXO_FIRECRACKER_BINARY")?.into(),
        jailer_bin: std::env::var("EXO_FIRECRACKER_JAILER")?.into(),
        image_size_gib: 1,
        workspace_size_gib: 1,
        network_device_policy: FirecrackerNetworkDevicePolicy::AllSandboxes,
        allow_guest_root: false,
        live_resource_growth: true,
        allowed_registries: vec!["docker.io".into()],
        ..Default::default()
    })
    .await?;
    let mut request = SandboxRequest {
        sandbox_id: "resource-growth".into(),
        scope: exoharness::ResourceScope::Global,
        provider_state: None,
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        spec: SandboxSpec {
            image: "docker.io/library/python:3.14-alpine".into(),
            resources: SandboxResourceShape::new(1, 512),
            default_workdir: "/workspace".into(),
            policy: SandboxNetworkPolicy::Disabled.into(),
            tcp_ports: vec![],
            mounts: vec![],
            durable_file_systems: vec![],
        },
    };
    let result = async {
        let mut handle = backend.acquire(request.clone()).await?;
        let mut process = handle
            .start_process(&command(&[
                "/bin/sh",
                "-ec",
                "printf ready; read line; printf '%s' \"$line\"",
            ]))
            .await?;
        let mut ready = [0; 5];
        process.stdout.read_exact(&mut ready).await?;
        assert_eq!(&ready, b"ready");
        let expanded = SandboxResourceShape::new(1, 1024).unwrap();
        for _ in 0..2 {
            assert_eq!(handle.expand_resources(expanded).await?, expanded);
        }
        assert!(
            handle
                .expand_resources(request.spec.resources.unwrap())
                .await
                .is_err()
        );
        assert_eq!(handle.live_resource_grant().await?, Some(expanded));
        process.stdin.write_all(b"retained\n").await?;
        process.stdin.close().await?;
        let mut retained = String::new();
        process.stdout.read_to_string(&mut retained).await?;
        assert_eq!(retained, "retained");
        assert_eq!(process.wait.await?, 0);
        let output = handle
            .exec(&command(&[
                "python3",
                "-c",
                r#"
import mmap
from pathlib import Path
m = mmap.mmap(-1, 768 * 1024**2, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
for p in range(0, len(m), 4096):
    m[p] = 173
assert all(m[p] == 173 for p in range(0, len(m), 4096))
Path('/tmp/growth-proof').write_text('root')
Path('/workspace/growth-proof').write_text('workspace')
print('verified-768-MiB')
"#,
            ]))
            .await?;
        anyhow::ensure!(
            output.ok && output.stdout.trim() == "verified-768-MiB",
            "expanded memory was not writable: {output:?}"
        );
        let mut snapshots = Vec::new();
        for kind in [SnapshotKind::Filesystem, SnapshotKind::Full] {
            let snapshot = if kind == SnapshotKind::Full {
                let snapshot = handle.snapshot_template(kind).await?;
                backend.terminate(request.clone()).await?;
                snapshot
            } else {
                backend.suspend(request.clone(), kind).await?
            };
            handle = backend
                .acquire_from_snapshot(request.clone(), snapshot.clone())
                .await?;
            assert_eq!(handle.live_resource_grant().await?, Some(expanded));
            let output = handle
                .exec(&command(&[
                    "/bin/sh",
                    "-ec",
                    "cat /tmp/growth-proof /workspace/growth-proof",
                ]))
                .await?;
            anyhow::ensure!(
                output.ok && output.stdout == "rootworkspace",
                "{kind:?} restore lost files: {output:?}"
            );
            request.provider_state = handle.provider_state();
            request.spec.image = handle.effective_image().context("missing restored image")?;
            snapshots.push(snapshot);
        }
        handle.stop().await?;
        for snapshot in snapshots {
            backend.delete_snapshot(snapshot).await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = backend.terminate(request).await;
    result?;
    cleanup?;
    Ok(())
}
