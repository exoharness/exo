use super::tests::{native_request, read_api_request, shell, test_request};
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
        allowed_registries: vec!["docker.io".into()],
        image_size_gib: 1,
        workspace_size_gib: 1,
        network_device_policy: FirecrackerNetworkDevicePolicy::AllSandboxes,
        live_resource_growth: true,
        max_machines: NonZeroUsize::new(1),
        ..Default::default()
    }
}

use super::balloon::Statistics;

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

fn assert_balloon(config: &FirecrackerConfig, record: &MachineRecord, mib: u32) -> Result<()> {
    let stats: Statistics = serde_json::from_slice(&firecracker_api_response(
        &jail_root(config, &record.machine_id).join("run/firecracker.socket"),
        "GET",
        "/balloon/statistics",
        &[],
        FIRECRACKER_API_TIMEOUT,
    )?)?;
    let pages = u64::from(mib) * 256;
    ensure!(
        stats.target_pages == pages && stats.actual_pages == pages,
        "expected {mib} MiB balloon: {stats:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires root, KVM, matching guest artifacts, and TMPDIR on XFS"]
async fn live_memory_capacity_changes_preserve_reattach_and_ram_restore() -> Result<()> {
    let temp = tempfile::Builder::new().prefix("cap-").tempdir()?;
    let image = std::env::var("FIRECRACKER_IMAGE")?;
    let mut config = config(temp.path(), &image);
    config.firecracker_bin = std::env::var("EXO_FIRECRACKER_BINARY")?.into();
    config.jailer_bin = std::env::var("EXO_FIRECRACKER_JAILER")?.into();
    let mut backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    ensure!(
        balloon::kvm_capability(balloon::KVM_CAP_MAX_VCPUS)? >= 32,
        "test needs 32 virtual CPUs"
    );
    Arc::get_mut(&mut backend.shared)
        .unwrap()
        .host_fingerprint
        .growth_capacity
        .as_mut()
        .unwrap()
        .resources
        .vcpu_count = std::num::NonZeroU8::new(32).unwrap();
    let mut request = test_request("capacity-source", 2048);
    request.spec.image = image;
    request.lifecycle.idle_ttl = Some(Duration::from_secs(600));
    request.spec.resources = SandboxResourceShape::new(1, 2048);
    let result: Result<()> = async {
        let handle = backend.acquire(request.clone()).await?;
        let original = allocation(&backend, &request).await?;
        assert_eq!(
            shell(&handle, "nproc; cat /sys/devices/system/cpu/present").await?,
            "1\n0-31\n"
        );
        let available = shell(&handle, "awk '/MemAvailable:/ {print $2}' /proc/meminfo")
            .await?
            .trim()
            .parse::<u64>()?;
        ensure!(
            available > 1024 * 1024,
            "CPU boot capacity consumed the initial grant: {available} KiB"
        );
        assert_eq!(
            fs::read_to_string(
                firecracker_cgroup_dir(&config, &original.machine_id).join("cpu.max")
            )?
            .trim(),
            "100000 100000"
        );
        let pid = fs::read(backend.shared.pid_path(&original.machine_id))?;
        shell(&handle, "printf retained > /dev/shm/capacity-proof").await?;
        request.provider_state = handle.provider_state();
        drop(handle);
        Arc::get_mut(&mut backend.shared)
            .context("backend unexpectedly shared")?
            .host_fingerprint
            .growth_capacity = Some(super::tests::test_growth_capacity(1, 512));
        let mut cold = request.clone();
        cold.sandbox_id = "capacity-cold".into();
        cold.provider_state = None;
        assert!(backend.acquire(cold).await.is_err());
        let handle = backend.acquire(request.clone()).await?;
        assert_eq!(
            fs::read(backend.shared.pid_path(&original.machine_id))?,
            pid
        );
        assert_eq!(
            allocation(&backend, &request).await?.runtime,
            original.runtime
        );
        assert_eq!(
            shell(&handle, "cat /dev/shm/capacity-proof; nproc").await?,
            "retained1\n"
        );
        let snapshot = handle.snapshot_template(crate::SnapshotKind::Full).await?;
        handle.stop().await?;
        drop(handle);
        Arc::get_mut(&mut backend.shared)
            .context("backend unexpectedly shared")?
            .host_fingerprint
            .growth_capacity = Some(super::tests::test_growth_capacity(2, 4096));
        request.sandbox_id = "capacity-restored".into();
        request.provider_state = None;
        let handle = backend
            .acquire_from_snapshot(request.clone(), snapshot.clone())
            .await?;
        assert_eq!(
            allocation(&backend, &request).await?.runtime,
            original.runtime
        );
        assert_eq!(
            shell(&handle, "cat /dev/shm/capacity-proof; nproc").await?,
            "retained1\n"
        );
        handle.stop().await?;
        backend.delete_snapshot(snapshot).await?;
        Ok(())
    }
    .await;
    backend.terminate_all().await?;
    result
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
    let exercise = async {
        let handle = backend.acquire(request.clone()).await?;
        request.provider_state = handle.provider_state();
        let capacity = handle
            .live_resource_ceiling()
            .context("missing growth capacity")?;
        ensure!(
            capacity.memory_mib.get() >= 8192,
            "test needs at least 8 GiB capacity"
        );
        let record = allocation(&backend, &request).await?;
        assert_balloon(&config, &record, 0)?;
        let before = shell(&handle, "awk '/MemAvailable:/ {print $2}' /proc/meminfo")
            .await?
            .trim()
            .parse::<u64>()?;
        assert!(before < 4096 * 1024);
        let cgroup = firecracker_cgroup_dir(&config, &record.machine_id);
        let original_limits = (
            fs::read(cgroup.join("memory.max"))?,
            fs::read(cgroup.join("memory.high"))?,
        );
        for shape in [
            (capacity.vcpu_count.get() + 1, 4096),
            (1, 2048),
            (1, capacity.memory_mib.get() + 1),
        ] {
            assert!(
                handle
                    .expand_resources(SandboxResourceShape::new(shape.0, shape.1).unwrap())
                    .await
                    .is_err()
            );
            assert_eq!(handle.live_resource_grant().await?, request.spec.resources);
            assert_balloon(&config, &record, 0)?;
            assert_eq!(
                (
                    fs::read(cgroup.join("memory.max"))?,
                    fs::read(cgroup.join("memory.high"))?
                ),
                original_limits
            );
        }
        let mut process = handle
            .start_process(&command("printf ready; read line; printf process:$line"))
            .await?;
        let mut ready = [0; 5];
        process.stdout.read_exact(&mut ready).await?;
        assert_eq!(&ready, b"ready");
        let mut terminal = handle
            .start_terminal(
                &command("stty -echo; printf ready; read line; printf terminal:$line"),
                SandboxTerminalSize { rows: 24, cols: 80 },
            )
            .await?;
        terminal.output.read_exact(&mut ready).await?;
        assert_eq!(&ready, b"ready");
        let mut server = handle
            .start_process(&command(
                r#"python3 -u -c 'import socket
s=socket.socket(); s.bind(("0.0.0.0",8765)); s.listen(); print("ready",flush=True)
c,_=s.accept()
while True:
 b=c.recv(1024)
 if not b: break
 c.sendall(b)'
"#,
            ))
            .await?;
        let mut server_ready = [0; 6];
        server.stdout.read_exact(&mut server_ready).await?;
        let mut connection = handle
            .connect_tcp(8765)
            .await?
            .context("missing TCP connection")?;
        tokio::io::AsyncWriteExt::write_all(&mut connection, b"before").await?;
        let mut echo = [0; 6];
        tokio::io::AsyncReadExt::read_exact(&mut connection, &mut echo).await?;
        assert_eq!(&echo, b"before");
        let grown = SandboxResourceShape::new(1, 8192).unwrap();
        let api = jail_root(&config, &record.machine_id).join("run/firecracker.socket");
        let parked = api.with_extension("parked");
        fs::rename(&api, &parked)?;
        let listener = StdUnixListener::bind(&api)?;
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let cgroup_for_peer = cgroup.clone();
        let peer = std::thread::spawn(move || -> Result<()> {
            let (mut stream, _) = listener.accept()?;
            let (request, _) = read_api_request(&mut stream)?;
            assert!(request.starts_with("PATCH /hotplug/memory "));
            entered.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(10))?;
            stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")?;
            let timeout = rustix::event::Timespec::try_from(FIRECRACKER_API_TIMEOUT)?;
            ensure!(
                rustix::event::poll(
                    &mut [rustix::event::PollFd::new(
                        &listener,
                        rustix::event::PollFlags::IN
                    )],
                    Some(&timeout),
                )? > 0,
                "warm acquire did not reach guest memory reconciliation"
            );
            let (mut stream, _) = listener.accept()?;
            let (request, _) = read_api_request(&mut stream)?;
            assert!(request.starts_with("PATCH /hotplug/memory "));
            let (max, high) = firecracker_memory_limits(grown)?;
            ensure!(
                fs::read_to_string(cgroup_for_peer.join("cpu.max"))?.trim() == "100000 100000",
                "warm acquire did not replay cpu.max"
            );
            let actual = fs::read_to_string(cgroup_for_peer.join("memory.max"))?;
            ensure!(
                actual.trim() == max.to_string(),
                "warm acquire reached guest growth with memory.max={actual}, expected {max}"
            );
            ensure!(
                fs::read_to_string(cgroup_for_peer.join("memory.high"))?.trim() == high.to_string(),
                "warm acquire did not replay memory.high"
            );
            stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")?;
            Ok(())
        });
        let growth = {
            let handle = Arc::clone(&handle);
            tokio::spawn(async move { handle.expand_resources(grown).await })
        };
        tokio::time::timeout(Duration::from_secs(2), waiting).await??;
        let concurrent = tokio::time::timeout(
            Duration::from_secs(2),
            shell(&handle, "printf during-growth"),
        )
        .await;
        release.send(())?;
        let failure = growth.await?;
        assert_eq!(concurrent??, "during-growth");
        assert!(
            matches!(failure?, crate::SandboxResourceUpdate::Incomplete {
            reservation,
            ..
        } if reservation == grown)
        );
        assert_eq!(
            fs::read_to_string(cgroup.join("cpu.max"))?.trim(),
            "100000 100000"
        );
        assert_eq!(handle.live_resource_grant().await?, Some(grown));
        // Recreate a crash after the journal write but before host limits changed.
        fs::write(cgroup.join("memory.max"), &original_limits.0)?;
        fs::write(cgroup.join("memory.high"), &original_limits.1)?;
        fs::write(cgroup.join("cpu.max"), "100000 100000")?;
        assert!(backend.acquire(request.clone()).await.is_err());
        peer.join().unwrap()?;
        fs::rename(&parked, &api)?;
        let recovered = backend.acquire(request.clone()).await?;
        assert_eq!(recovered.live_resource_grant().await?, Some(grown));
        assert_balloon(&config, &record, 0)?;
        assert!(
            handle
                .expand_resources(SandboxResourceShape::new(1, 4096).unwrap())
                .await
                .is_err()
        );
        assert_eq!(
            shell(&handle, "printf usable-after-failure").await?,
            "usable-after-failure"
        );
        assert!(
            matches!(handle.expand_resources(grown).await?, crate::SandboxResourceUpdate::Applied { grant } if grant == grown)
        );
        assert!(
            matches!(handle.expand_resources(grown).await?, crate::SandboxResourceUpdate::Applied { grant } if grant == grown)
        );
        assert_eq!(handle.live_resource_grant().await?, Some(grown));
        assert_balloon(&config, &record, 0)?;
        assert_eq!(
            fs::read_to_string(cgroup.join("memory.max"))?.trim(),
            (9216_u64 * 1024 * 1024).to_string()
        );
        let after = shell(&handle, "awk '/MemAvailable:/ {print $2}' /proc/meminfo")
            .await?
            .trim()
            .parse::<u64>()?;
        assert!(
            after > before + 3 * 1024 * 1024,
            "before={before}, after={after}"
        );
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
        let snapshot = backend
            .suspend(request.clone(), crate::SnapshotKind::Filesystem)
            .await?;
        assert_eq!(handle.is_running().await?, Some(false));
        let restored = backend
            .acquire_from_snapshot(request.clone(), snapshot.clone())
            .await?;
        let restored_record = allocation(&backend, &request).await?;
        assert_eq!(restored.live_resource_grant().await?, Some(grown));
        assert_eq!(
            fs::read_to_string(
                firecracker_cgroup_dir(&config, &restored_record.machine_id).join("cpu.max")
            )?
            .trim(),
            "100000 100000"
        );
        let available = shell(&restored, "awk '/MemAvailable:/ {print $2}' /proc/meminfo")
            .await?
            .trim()
            .parse::<u64>()?;
        assert!(
            available > 7 * 1024 * 1024,
            "restored available memory: {available}"
        );
        assert_eq!(
            shell(
                &restored,
                "cat /tmp/root-state /workspace/state /repo/state"
            )
            .await?,
            "rootworkspacerepository"
        );
        request.spec.image = restored
            .effective_image()
            .context("missing restored image")?;
        request.provider_state = restored.provider_state();
        let reattached = backend.acquire(request.clone()).await?;
        let reattached_record = allocation(&backend, &request).await?;
        ensure!(
            reattached_record.machine_id == restored_record.machine_id,
            "reacquisition replaced the restored VM"
        );
        assert_eq!(reattached.live_resource_grant().await?, Some(grown));
        assert_eq!(
            shell(
                &reattached,
                "cat /tmp/root-state /workspace/state /repo/state"
            )
            .await?,
            "rootworkspacerepository"
        );
        let recaptured = reattached.snapshot(crate::SnapshotKind::Filesystem).await?;
        backend.delete_snapshot(recaptured).await?;
        reattached.stop().await?;
        backend.delete_snapshot(snapshot).await?;
        Ok(())
    };
    let result: Result<()> = exercise.await;
    backend.terminate_all().await?;
    result
}

fn assert_resident_snapshots(config: &FirecrackerConfig, record: &MachineRecord) -> Result<()> {
    #[derive(Deserialize)]
    struct Tracking {
        track_dirty_pages: bool,
    }
    let tracking: Tracking = serde_json::from_slice(&firecracker_api_response(
        &jail_root(config, &record.machine_id).join("run/firecracker.socket"),
        "GET",
        "/machine-config",
        &[],
        FIRECRACKER_API_TIMEOUT,
    )?)?;
    ensure!(
        !tracking.track_dirty_pages,
        "live growth enabled KVM dirty logging"
    );
    let swap_max = fs::read_to_string(
        firecracker_cgroup_dir(config, &record.machine_id).join("memory.swap.max"),
    )?;
    ensure!(
        swap_max.trim() == "0",
        "resident-page snapshots require host swap to be disabled"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires root, KVM, Python guest image, and TMPDIR on XFS"]
async fn live_memory_ram_fork_and_restore_retain_grant_and_identity() -> Result<()> {
    let temp = tempfile::Builder::new().prefix("m").tempdir()?;
    let image = std::env::var("FIRECRACKER_IMAGE")?;
    let mut config = config(temp.path(), &image);
    config.max_machines = NonZeroUsize::new(2);
    let backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    let mut source = test_request("ram-source", 512);
    source.spec.image = image;
    source.spec.default_workdir = "/workspace".into();
    source.lifecycle.idle_ttl = Some(Duration::from_secs(600));
    let mut target = source.clone();
    target.sandbox_id = "ram-target".into();
    let exercise = async {
        let handle = backend.acquire(source.clone()).await?;
        assert_resident_snapshots(&config, &allocation(&backend, &source).await?)?;
        assert!(matches!(
            handle
                .expand_resources(SandboxResourceShape::new(1, 1025).unwrap())
                .await?,
            crate::SandboxResourceUpdate::Applied { .. }
        ));
        // tmpfs contents exist only in guest RAM, so disk capture cannot make
        // these checks pass if the RAM snapshot misses resident or COW pages.
        shell(
            &handle,
            r#"python3 -c '
from pathlib import Path
Path("/dev/shm/ram-state").write_bytes(bytes([17]) * 16 * 1024**2)
'"#,
        )
        .await?;
        let check = r#"python3 -c '
import hashlib
with open("/dev/shm/ram-state", "rb") as f:
    print(hashlib.sha256(f.read()).hexdigest(), end="")
'"#;
        let update = |value| {
            format!(
                r#"python3 -c '
with open("/dev/shm/ram-state", "r+b") as f:
    for offset in [0, 4096, 16*1024**2-1]:
        f.seek(offset)
        f.write(bytes([{value}]))
'"#
            )
        };
        let mut expected = vec![17u8; 16 * 1024 * 1024];
        let original = format!("{:x}", Sha256::digest(&expected));
        assert_eq!(shell(&handle, check).await?, original);
        let fork = backend.fork_sandbox(source.clone(), target.clone()).await?;
        let fork_record = allocation(&backend, &target).await?;
        assert_resident_snapshots(&config, &fork_record)?;
        assert_eq!(
            fork.live_resource_grant().await?,
            SandboxResourceShape::new(1, 1025)
        );
        ensure!(
            fork_record.runtime.memory_mib == 512,
            "fork changed the sandbox identity"
        );
        assert_eq!(shell(&fork, check).await?, original);
        shell(&handle, &update(34)).await?;
        for offset in [0, 4096, expected.len() - 1] {
            expected[offset] = 34;
        }
        let source_digest = format!("{:x}", Sha256::digest(&expected));
        assert_eq!(shell(&handle, check).await?, source_digest);
        assert_eq!(shell(&fork, check).await?, original);
        target.provider_state = fork.provider_state();
        target.spec.image = fork.effective_image().context("missing fork image")?;
        let attached = backend.acquire(target.clone()).await?;
        ensure!(
            allocation(&backend, &target).await?.machine_id == fork_record.machine_id,
            "reacquisition replaced the fork"
        );
        // Change RAM in a VM restored from the first capture. A second capture
        // must preserve these private COW writes as well as untouched RAM.
        shell(&attached, &update(51)).await?;
        for offset in [0, 4096, expected.len() - 1] {
            expected[offset] = 51;
        }
        let changed = format!("{:x}", Sha256::digest(&expected));
        assert_eq!(shell(&attached, check).await?, changed);
        let snapshot = attached.snapshot(crate::SnapshotKind::Full).await?;
        backend.terminate(target.clone()).await?;
        let restored = backend
            .acquire_from_snapshot(target.clone(), snapshot.clone())
            .await?;
        let restored_record = allocation(&backend, &target).await?;
        assert_resident_snapshots(&config, &restored_record)?;
        assert_eq!(
            restored.live_resource_grant().await?,
            SandboxResourceShape::new(1, 1025)
        );
        assert_balloon(&config, &restored_record, 1)?;
        assert_eq!(shell(&restored, check).await?, changed);
        assert_eq!(shell(&handle, check).await?, source_digest);
        // Resident-page diffs may include clean pages across a mapped slot;
        // the unused node capacity must still stay sparse.
        for entry in fs::read_dir(config.state_root.join("snapshots"))? {
            let memory = entry?.path().join("memory");
            if memory.try_exists()? {
                let metadata = memory.metadata()?;
                let allocated = metadata.blocks() * 512;
                ensure!(
                    allocated < 2 * 1024 * 1024 * 1024,
                    "RAM capture allocated {} bytes for logical {} bytes",
                    allocated,
                    metadata.len()
                );
            }
        }
        restored.stop().await?;
        backend.delete_snapshot(snapshot).await?;
        Ok(())
    };
    let result: Result<()> = exercise.await;
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
        assert_balloon(&config, &record, 0)?;

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
        assert!(matches!(
            growth?,
            crate::SandboxResourceUpdate::Applied { .. }
        ));
        let capture = capture?;
        assert_eq!(
            balloon::grant(&allocation(&backend, &request).await?)?,
            8192
        );
        assert_balloon(&config, &record, 0)?;
        assert_eq!(shell(&handle, "printf usable").await?, "usable");
        backend.delete_snapshot(capture).await?;
        let (growth, stop) = tokio::join!(
            handle.expand_resources(SandboxResourceShape::new(1, 8192).unwrap()),
            handle.stop(),
        );
        stop?;
        if let Ok(update) = &growth {
            assert!(matches!(
                update,
                crate::SandboxResourceUpdate::Applied { .. }
            ));
        }
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
    let mut backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    // A node-sized, unplugged range must not consume the small initial grant.
    Arc::get_mut(&mut backend.shared)
        .context("backend unexpectedly shared")?
        .host_fingerprint
        .growth_capacity
        .as_mut()
        .unwrap()
        .resources
        .memory_mib = std::num::NonZeroU32::new(262144).unwrap();
    let mut request = native_request(&config, "allocation", &image)?;
    request.spec.resources = SandboxResourceShape::new(1, 2048);
    let result: Result<()> = async {
        let handle = backend.acquire(request).await?;
        assert_eq!(
            handle.live_resource_grant().await?,
            SandboxResourceShape::new(1, 2048)
        );
        assert_eq!(
            handle.live_resource_ceiling(),
            SandboxResourceShape::new(1, 262144)
        );
        let available = shell(&handle, "awk '/MemAvailable:/ {print $2}' /proc/meminfo")
            .await?
            .trim()
            .parse::<u64>()?;
        ensure!(
            available > 1024 * 1024,
            "large capacity consumed the initial grant: {available} KiB"
        );
        assert!(matches!(
            handle
                .expand_resources(SandboxResourceShape::new(1, 8192).unwrap())
                .await?,
            crate::SandboxResourceUpdate::Applied { .. }
        ));
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
#[ignore = "requires root, KVM, Python guest image, XFS TMPDIR"]
async fn live_memory_small_grant_is_writable() -> Result<()> {
    let temp = tempfile::Builder::new().prefix("mem-").tempdir()?;
    let image = std::env::var("FIRECRACKER_IMAGE")?;
    let mut config = config(temp.path(), &image);
    config.network_device_policy = FirecrackerNetworkDevicePolicy::EnabledSandboxes;
    let backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    let mut request = native_request(&config, "small-grant", &image)?;
    request.spec.resources = SandboxResourceShape::new(1, 512);
    let result: Result<()> = async {
        let handle = backend.acquire(request.clone()).await?;
        let record = allocation(&backend, &request).await?;
        let available = shell(&handle, "awk '/MemAvailable:/ {print $2}' /proc/meminfo")
            .await?
            .trim()
            .parse::<u64>()?;
        assert!(
            available >= 256 * 1024,
            "insufficient usable memory: {available} KiB"
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
        let current = fs::read_to_string(cgroup.join("memory.current"))?
            .trim()
            .parse::<u64>()?;
        let events = fs::read_to_string(cgroup.join("memory.events"))?;
        assert!(current < high, "balloon memory exceeds grant headroom");
        assert_eq!(
            fs::read_to_string(cgroup.join("memory.max"))?
                .trim()
                .parse::<u64>()?,
            u64::from(
                backend
                    .host_resource_reservation(request.spec.resources.unwrap())?
                    .memory_mib
                    .get()
            ) * 1024
                * 1024
        );
        assert_eq!(
            fs::read_to_string(cgroup.join("memory.swap.max"))?.trim(),
            "0"
        );
        for key in ["high", "max", "oom", "oom_kill"] {
            assert!(
                events.lines().any(|line| line == format!("{key} 0")),
                "{events}"
            );
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

#[tokio::test]
#[ignore = "requires root, KVM, Firecracker artifacts, and TMPDIR on XFS"]
async fn live_memory_shared_ceiling_accepts_small_and_odd_sized_requests() -> Result<()> {
    let temp = tempfile::Builder::new().prefix("shapes-").tempdir()?;
    let image = std::env::var("FIRECRACKER_IMAGE")?;
    let config = config(temp.path(), &image);
    let backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    let result: Result<()> = async {
        for memory in [384, 2051] {
            let mut request = native_request(&config, &format!("shape-{memory}"), &image)?;
            request.spec.resources = SandboxResourceShape::new(1, memory);
            let handle = backend.acquire(request.clone()).await?;
            let record = allocation(&backend, &request).await?;
            assert_eq!(handle.live_resource_grant().await?, request.spec.resources);
            assert_eq!(
                handle.live_resource_ceiling(),
                SandboxResourceShape::new(
                    1,
                    backend
                        .shared
                        .host_fingerprint
                        .growth_capacity
                        .unwrap()
                        .resources
                        .memory_mib
                        .get()
                )
            );
            assert_balloon(&config, &record, memory % 2)?;
            assert_eq!(shell(&handle, "printf usable-shape").await?, "usable-shape");
            handle.stop().await?;
        }
        Ok(())
    }
    .await;
    backend.terminate_all().await?;
    result
}
