use super::tests::{native_request, shell};
use super::*;
use futures::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn command(script: &str) -> SandboxCommand {
    SandboxCommand {
        argv: vec!["/bin/sh".into(), "-ec".into(), script.into()],
        env: HashMap::new(),
        display_argv: None,
        cwd: Some("/".into()),
        timeout: Some(Duration::from_secs(60)),
    }
}

fn config(root: &Path, image: &str) -> FirecrackerConfig {
    FirecrackerConfig {
        state_root: root.join("s"),
        allowed_local_images: vec![image.into()],
        image_size_gib: 1,
        workspace_size_gib: 1,
        network_device_policy: FirecrackerNetworkDevicePolicy::AllSandboxes,
        memory_ceiling_mib: std::num::NonZeroU32::new(8192),
        max_machines: NonZeroUsize::new(1),
        ..Default::default()
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Statistics {
    target_pages: u64,
    actual_pages: u64,
}

async fn allocation(
    backend: &FirecrackerSandboxBackend,
    request: &SandboxRequest,
) -> Result<MachineRecord> {
    let id = backend.shared.warm_machines.lock().await[&request.sandbox_id]
        .machine_id
        .clone();
    backend
        .shared
        .load_machine_record(&id)
        .await?
        .context("missing allocation")
}

fn statistics(config: &FirecrackerConfig, record: &MachineRecord) -> Result<Statistics> {
    serde_json::from_slice(&firecracker_api_response(
        &jail_root(config, &record.machine_id).join("run/firecracker.socket"),
        "GET",
        "/balloon/statistics",
        &(),
        FIRECRACKER_API_TIMEOUT,
    )?)
    .map_err(Into::into)
}

#[tokio::test]
#[ignore = "requires root, KVM, ARM64/x86_64 artifacts, Python guest image, and TMPDIR on XFS"]
async fn live_memory_growth_preserves_io_and_filesystem_restore() -> Result<()> {
    let temp = tempfile::Builder::new().prefix("m").tempdir()?;
    let image = std::env::var("FIRECRACKER_IMAGE")?;
    let config = config(temp.path(), &image);
    let backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    let mut request = native_request(&config, "growth", &image)?;
    request.spec.resources = SandboxResourceShape::new(1, 4096);
    request.spec.tcp_ports = vec![8765];
    let result: Result<()> = async {
        let handle = backend.acquire(request.clone()).await?;
        eprintln!("acquired at 4 GiB with initial inflation complete");
        request.provider_state = handle.provider_state();
        assert_eq!(handle.live_resource_ceiling(), SandboxResourceShape::new(1, 8192));
        let record = allocation(&backend, &request).await?;
        assert_eq!(statistics(&config, &record)?, Statistics { target_pages: 4096 * 256, actual_pages: 4096 * 256 });
        let before = shell(&handle, "awk '/MemAvailable:/ {print $2}' /proc/meminfo").await?.trim().parse::<u64>()?;
        assert!(before < 4096 * 1024);
        let cgroup = firecracker_cgroup_dir(&config, &record.machine_id);
        let original_limits = (fs::read(cgroup.join("memory.max"))?, fs::read(cgroup.join("memory.high"))?);
        for shape in [(2, 4096), (1, 2048), (1, 8193)] {
            assert!(handle.expand_resources(SandboxResourceShape::new(shape.0, shape.1).unwrap()).await.is_err());
            assert_eq!(balloon::grant(&allocation(&backend, &request).await?)?, 4096);
            assert_eq!(statistics(&config, &record)?, Statistics { target_pages: 4096 * 256, actual_pages: 4096 * 256 });
            assert_eq!((fs::read(cgroup.join("memory.max"))?, fs::read(cgroup.join("memory.high"))?), original_limits);
        }
        let mut process = handle.start_process(&command("printf ready; read line; printf process:$line")).await?;
        let mut ready = [0; 5];
        process.stdout.read_exact(&mut ready).await?;
        assert_eq!(&ready, b"ready");
        let mut terminal = handle.start_terminal(&command("stty -echo; printf ready; read line; printf terminal:$line"), SandboxTerminalSize { rows: 24, cols: 80 }).await?;
        terminal.output.read_exact(&mut ready).await?;
        assert_eq!(&ready, b"ready");
        let mut server = handle.start_process(&command(r#"python3 -u -c 'import socket
s=socket.socket(); s.bind(("0.0.0.0",8765)); s.listen(); print("ready",flush=True)
c,_=s.accept()
while True:
 b=c.recv(1024)
 if not b: break
 c.sendall(b)'
"#)).await?;
        let mut server_ready = [0; 6];
        server.stdout.read_exact(&mut server_ready).await?;
        let mut connection = handle.connect_tcp(8765).await?.context("missing TCP connection")?;
        tokio::io::AsyncWriteExt::write_all(&mut connection, b"before").await?;
        let mut echo = [0; 6];
        tokio::io::AsyncReadExt::read_exact(&mut connection, &mut echo).await?;
        assert_eq!(&echo, b"before");
        let grown = SandboxResourceShape::new(1, 8192).unwrap();
        let api = jail_root(&config, &record.machine_id).join("run/firecracker.socket");
        let parked = api.with_extension("parked");
        fs::rename(&api, &parked)?;
        let failure = handle.expand_resources(grown).await;
        fs::rename(&parked, &api)?;
        assert!(failure.is_err());
        assert_eq!(handle.live_resource_grant().await?, Some(grown));
        assert_eq!(statistics(&config, &record)?, Statistics { target_pages: 4096 * 256, actual_pages: 4096 * 256 });
        assert!(handle.expand_resources(SandboxResourceShape::new(1, 4096).unwrap()).await.is_err());
        assert_eq!(shell(&handle, "printf usable-after-failure").await?, "usable-after-failure");
        assert_eq!(handle.expand_resources(grown).await?, grown);
        assert_eq!(handle.expand_resources(grown).await?, grown);
        assert_eq!(balloon::grant(&allocation(&backend, &request).await?)?, 8192);
        assert_eq!(statistics(&config, &record)?, Statistics { target_pages: 0, actual_pages: 0 });
        assert_eq!(fs::read_to_string(cgroup.join("memory.max"))?.trim(), (9216_u64 * 1024 * 1024).to_string());
        let after = shell(&handle, "awk '/MemAvailable:/ {print $2}' /proc/meminfo").await?.trim().parse::<u64>()?;
        assert!(after > before + 3 * 1024 * 1024, "before={before}, after={after}");
        eprintln!("grew to 8 GiB with additional memory available");
        process.stdin.write_all(b"alive\n").await?;
        process.stdin.flush().await?;
        let mut output = String::new();
        process.stdout.read_to_string(&mut output).await?;
        assert_eq!(output, "process:alive");
        assert_eq!(process.wait.await?, 0);
        terminal.input.write_all(b"alive\n").await?;
        terminal.input.flush().await?;
        let mut output = String::new();
        terminal.output.read_to_string(&mut output).await?;
        assert_eq!(output.trim(), "terminal:alive");
        assert_eq!(terminal.wait.await?, 0);
        tokio::io::AsyncWriteExt::write_all(&mut connection, b"after!").await?;
        tokio::io::AsyncReadExt::read_exact(&mut connection, &mut echo).await?;
        assert_eq!(&echo, b"after!");
        drop(connection);
        server.terminate().await?;
        shell(&handle, "printf root > /tmp/root-state; printf workspace > /workspace/state; printf repository > /repo/state").await?;
        let snapshot = backend.suspend(request.clone(), crate::SnapshotKind::Filesystem).await?;
        eprintln!("filesystem suspension completed");
        assert_eq!(handle.is_running().await?, Some(false));
        let restored = backend.acquire_from_snapshot(request.clone(), snapshot.clone()).await?;
        eprintln!("restored filesystem capture");
        let restored_record = allocation(&backend, &request).await?;
        ensure!(balloon::grant(&restored_record)? == 8192, "filesystem restore lost the expanded memory grant");
        let available = shell(&restored, "awk '/MemAvailable:/ {print $2}' /proc/meminfo").await?.trim().parse::<u64>()?;
        assert!(available > 7 * 1024 * 1024, "restored available memory: {available}");
        assert_eq!(shell(&restored, "cat /tmp/root-state /workspace/state /repo/state").await?, "rootworkspacerepository");
        request.spec.image = restored.effective_image().context("missing restored image")?;
        request.provider_state = restored.provider_state();
        let reattached = backend.acquire(request.clone()).await?;
        let reattached_record = allocation(&backend, &request).await?;
        ensure!(reattached_record.machine_id == restored_record.machine_id, "reacquisition replaced the restored VM");
        ensure!(balloon::grant(&reattached_record)? == 8192, "reacquisition lost the expanded memory grant");
        assert_eq!(shell(&reattached, "cat /tmp/root-state /workspace/state /repo/state").await?, "rootworkspacerepository");
        let recaptured = reattached.snapshot(crate::SnapshotKind::Filesystem).await?;
        backend.delete_snapshot(recaptured).await?;
        reattached.stop().await?;
        backend.delete_snapshot(snapshot).await?;
        Ok(())
    }.await;
    backend.terminate_all().await?;
    result
}

#[tokio::test]
#[ignore = "requires root, KVM, Firecracker artifacts, and TMPDIR on XFS"]
async fn live_memory_ram_fork_and_restore_retain_grant_and_identity() -> Result<()> {
    let temp = tempfile::Builder::new().prefix("m").tempdir()?;
    let image = std::env::var("FIRECRACKER_IMAGE")?;
    let mut config = config(temp.path(), &image);
    config.memory_ceiling_mib = std::num::NonZeroU32::new(1024);
    config.max_machines = NonZeroUsize::new(2);
    let backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    let source = SandboxRequest {
        sandbox_id: "ram-source".into(),
        scope: crate::ResourceScope::Global,
        provider_state: None,
        lifecycle: crate::SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(600)),
        },
        spec: SandboxSpec {
            image,
            resources: SandboxResourceShape::new(1, 512),
            default_workdir: "/workspace".into(),
            policy: SandboxNetworkPolicy::Disabled.into(),
            tcp_ports: vec![],
            mounts: vec![],
            durable_file_systems: vec![],
        },
    };
    let mut target = source.clone();
    target.sandbox_id = "ram-target".into();
    let result: Result<()> = async {
        let handle = backend.acquire(source.clone()).await?;
        handle
            .expand_resources(SandboxResourceShape::new(1, 1024).unwrap())
            .await?;
        shell(&handle, "printf retained > /tmp/ram-state").await?;
        let fork = backend.fork_sandbox(source.clone(), target.clone()).await?;
        let fork_record = allocation(&backend, &target).await?;
        ensure!(
            balloon::grant(&fork_record)? == 1024,
            "fork lost the memory grant"
        );
        ensure!(
            fork_record.runtime.memory_mib == 512,
            "fork changed the sandbox identity"
        );
        assert_eq!(shell(&fork, "cat /tmp/ram-state").await?, "retained");
        target.provider_state = fork.provider_state();
        target.spec.image = fork.effective_image().context("missing fork image")?;
        let attached = backend.acquire(target.clone()).await?;
        ensure!(
            allocation(&backend, &target).await?.machine_id == fork_record.machine_id,
            "reacquisition replaced the fork"
        );
        let snapshot = attached.snapshot(crate::SnapshotKind::Full).await?;
        backend.terminate(target.clone()).await?;
        let restored = backend
            .acquire_from_snapshot(target.clone(), snapshot.clone())
            .await?;
        let restored_record = allocation(&backend, &target).await?;
        ensure!(
            balloon::grant(&restored_record)? == 1024,
            "RAM restore lost the memory grant"
        );
        assert_eq!(
            statistics(&config, &restored_record)?,
            Statistics {
                target_pages: 0,
                actual_pages: 0
            }
        );
        assert_eq!(shell(&restored, "cat /tmp/ram-state").await?, "retained");
        restored.stop().await?;
        backend.delete_snapshot(snapshot).await?;
        Ok(())
    }
    .await;
    backend.terminate_all().await?;
    result
}

#[tokio::test]
#[ignore = "requires root, KVM, Python guest image, and TMPDIR on XFS"]
async fn live_memory_disconnect_capture_and_shutdown_races() -> Result<()> {
    let temp = tempfile::Builder::new().prefix("m").tempdir()?;
    let image = std::env::var("FIRECRACKER_IMAGE")?;
    let config = config(temp.path(), &image);
    let backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    let mut request = native_request(&config, "races", &image)?;
    request.spec.resources = SandboxResourceShape::new(1, 4096);
    let result: Result<()> = async {
        let handle = backend.acquire(request.clone()).await?;
        request.provider_state = handle.provider_state();
        let record = allocation(&backend, &request).await?;
        let grown = SandboxResourceShape::new(1, 6144).unwrap();
        // Disconnect immediately after dispatch, before the detached task is
        // polled. Reconciliation must wait for its committed grant.
        let mut expansion = Box::pin(handle.expand_resources(grown));
        assert!(futures::poll!(&mut expansion).is_pending());
        drop(expansion);
        assert_eq!(handle.live_resource_grant().await?, Some(grown));
        assert_eq!(
            statistics(&config, &record)?,
            Statistics {
                target_pages: 2048 * 256,
                actual_pages: 2048 * 256
            }
        );

        // Cancellation while waiting for ownership must not dispatch growth.
        let guard = backend
            .shared
            .lifecycle_locks
            .lock_machine(&record.machine_id)
            .await;
        let mut waiting =
            Box::pin(handle.expand_resources(SandboxResourceShape::new(1, 8192).unwrap()));
        assert!(futures::poll!(&mut waiting).is_pending());
        drop(waiting);
        drop(guard);
        assert_eq!(handle.live_resource_grant().await?, Some(grown));
        let (growth, capture) = tokio::join!(
            handle.expand_resources(SandboxResourceShape::new(1, 8192).unwrap()),
            handle.snapshot(crate::SnapshotKind::Filesystem),
        );
        growth?;
        let capture = capture?;
        assert_eq!(
            balloon::grant(&allocation(&backend, &request).await?)?,
            8192
        );
        assert_eq!(
            statistics(&config, &record)?,
            Statistics {
                target_pages: 0,
                actual_pages: 0
            }
        );
        assert_eq!(shell(&handle, "printf usable").await?, "usable");
        backend.delete_snapshot(capture).await?;
        let (growth, stop) = tokio::join!(
            handle.expand_resources(SandboxResourceShape::new(1, 8192).unwrap()),
            handle.stop(),
        );
        stop?;
        if let Err(error) = growth {
            assert!(
                error.to_string().contains("not running") || error.to_string().contains("missing"),
                "{error:#}"
            );
        }
        assert_eq!(handle.is_running().await?, Some(false));
        ensure!(
            backend
                .shared
                .load_machine_record(&record.machine_id)
                .await?
                .is_none(),
            "stop retained the old allocation"
        );
        request.spec.resources = SandboxResourceShape::new(1, 8192);
        let resumed = backend.acquire(request.clone()).await?;
        let resumed_record = allocation(&backend, &request).await?;
        assert_eq!(balloon::grant(&resumed_record)?, 8192);
        assert_eq!(shell(&resumed, "printf usable").await?, "usable");
        Ok(())
    }
    .await;
    backend.terminate_all().await?;
    result
}

#[tokio::test]
#[ignore = "requires root, KVM, Python guest image, and TMPDIR on XFS; slow on nested KVM"]
async fn live_memory_growth_makes_six_gib_writable() -> Result<()> {
    let temp = tempfile::Builder::new().prefix("m").tempdir()?;
    let image = std::env::var("FIRECRACKER_IMAGE")?;
    let mut config = config(temp.path(), &image);
    config.network_device_policy = FirecrackerNetworkDevicePolicy::EnabledSandboxes;
    let backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    let mut request = native_request(&config, "allocation", &image)?;
    request.spec.resources = SandboxResourceShape::new(1, 4096);
    let result: Result<()> = async {
        let handle = backend.acquire(request).await?;
        handle
            .expand_resources(SandboxResourceShape::new(1, 8192).unwrap())
            .await?;
        // Fault in all 6 GiB and verify a nonzero byte in every page of guest RAM.
        // Reading MemTotal alone cannot prove that ballooned pages became usable.
        let mut probe = command(
            r#"python3 -c '
import mmap
n = 6 * 1024**3
m = mmap.mmap(-1, n)
m[::4096] = bytes([173]) * (n // 4096)
assert m[::4096] == bytes([173]) * (n // 4096)
print("wrote-and-verified-6-GiB")'
"#,
        );
        probe.timeout = Some(Duration::from_secs(3600));
        let output = handle.exec(&probe).await?;
        ensure!(
            output.ok && output.stdout.trim() == "wrote-and-verified-6-GiB",
            "memory probe: {output:?}"
        );
        Ok(())
    }
    .await;
    backend.terminate_all().await?;
    result
}

#[tokio::test]
#[ignore = "requires root, KVM, Python guest image, XFS TMPDIR; samples THP accounting for 60 seconds"]
async fn live_memory_small_grant_host_accounting() -> Result<()> {
    let temp = tempfile::Builder::new().prefix("acct-").tempdir()?;
    let image = std::env::var("FIRECRACKER_IMAGE")?;
    let mut config = config(temp.path(), &image);
    config.memory_ceiling_mib = std::num::NonZeroU32::new(2048);
    config.network_device_policy = FirecrackerNetworkDevicePolicy::EnabledSandboxes;
    let backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    let mut request = native_request(&config, "accounting", &image)?;
    request.spec.resources = SandboxResourceShape::new(1, 512);
    let result: Result<()> = async {
        let started = Instant::now();
        let handle = backend.acquire(request.clone()).await?;
        let record = allocation(&backend, &request).await?;
        let available = shell(&handle, "awk '/MemAvailable:/ {print $2}' /proc/meminfo")
            .await?
            .trim()
            .parse::<u64>()?;
        eprintln!(
            "512 MiB grant / 2 GiB ceiling: acquire_ms={} available_kib={available}",
            started.elapsed().as_millis()
        );
        assert!(
            available >= 256 * 1024,
            "insufficient usable memory: {available} KiB"
        );

        // Admission must reject an unusable shape without replacing the live VM.
        let mut invalid = request.clone();
        invalid.provider_state = handle.provider_state();
        invalid.spec.resources = SandboxResourceShape::new(1, 128);
        assert!(
            backend
                .acquire(invalid)
                .await
                .err()
                .context("invalid ceiling accepted")?
                .to_string()
                .contains("four times")
        );
        assert_eq!(
            allocation(&backend, &request).await?.machine_id,
            record.machine_id
        );
        let mut workload = handle
            .start_process(&command(
                r#"python3 -u -c '
import mmap,sys
m=mmap.mmap(-1,192*1024**2)
m[::4096]=bytes([173])*(len(m)//4096)
print("ready",flush=True)
sys.stdin.readline()
assert m[::4096]==bytes([173])*(len(m)//4096)
print("verified")'
"#,
            ))
            .await?;
        let mut ready = [0; 6];
        workload.stdout.read_exact(&mut ready).await?;
        assert_eq!(&ready, b"ready\n");
        let cgroup = firecracker_cgroup_dir(&config, &record.machine_id);
        let high = fs::read_to_string(cgroup.join("memory.high"))?
            .trim()
            .parse::<u64>()?;
        for sample in 0..=6 {
            if sample > 0 {
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            let current = fs::read_to_string(cgroup.join("memory.current"))?
                .trim()
                .parse::<u64>()?;
            let events = fs::read_to_string(cgroup.join("memory.events"))?;
            eprintln!(
                "accounting sample={sample} memory.current={current} memory.high={high} {}",
                events.replace('\n', "; ")
            );
            assert!(current < high, "balloon/THP memory exceeds grant headroom");
            for key in ["high", "max", "oom", "oom_kill"] {
                assert!(
                    events.lines().any(|line| line == format!("{key} 0")),
                    "{events}"
                );
            }
        }
        workload.stdin.write_all(b"verify\n").await?;
        workload.stdin.close().await?;
        let mut output = String::new();
        workload.stdout.read_to_string(&mut output).await?;
        assert_eq!(output, "verified\n");
        assert_eq!(workload.wait.await?, 0);
        Ok(())
    }
    .await;
    let cleanup = backend.terminate_all().await;
    result?;
    cleanup?;
    Ok(())
}
