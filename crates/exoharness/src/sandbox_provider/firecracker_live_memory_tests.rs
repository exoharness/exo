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

// Write all 6 GiB and verify one byte in every page of anonymous guest RAM.
// Reading MemTotal alone cannot prove that ballooned pages became usable.
async fn write_six_gib(handle: &Arc<dyn ManagedSandboxHandle>) -> Result<()> {
    let mut probe = command(
        r#"python3 -c '
import ctypes, mmap
n = 6 * 1024**3
m = mmap.mmap(-1, n, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
ctypes.memset(ctypes.addressof(ctypes.c_char.from_buffer(m)), 173, n)
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
            assert_eq!(balloon::grant(&config, &record)?, 4096);
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
        assert_eq!(handle.expand_resources(grown).await?, grown);
        assert_eq!(handle.expand_resources(grown).await?, grown);
        assert_eq!(balloon::grant(&config, &record)?, 8192);
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
        ensure!(balloon::grant(&config, &restored_record)? == 8192, "filesystem restore lost the expanded memory grant");
        let available = shell(&restored, "awk '/MemAvailable:/ {print $2}' /proc/meminfo").await?.trim().parse::<u64>()?;
        assert!(available > 7 * 1024 * 1024, "restored available memory: {available}");
        assert_eq!(shell(&restored, "cat /tmp/root-state /workspace/state /repo/state").await?, "rootworkspacerepository");
        restored.stop().await?;
        backend.delete_snapshot(snapshot).await?;
        Ok(())
    }.await;
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
        // Hold the real lifecycle lock until the caller has disconnected. The
        // detached expansion must finish once the lock is released.
        let guard = backend
            .shared
            .lifecycle_locks
            .lock_machine(&record.machine_id)
            .await;
        let disconnected_handle = Arc::clone(&handle);
        let caller = tokio::spawn(async move {
            disconnected_handle
                .expand_resources(SandboxResourceShape::new(1, 6144).unwrap())
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        drop(guard);
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if balloon::grant(&config, &record)? == 6144
                    && statistics(&config, &record)?.actual_pages == 2048 * 256
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("disconnected growth did not finish")??;
        let (growth, capture) = tokio::join!(
            handle.expand_resources(SandboxResourceShape::new(1, 8192).unwrap()),
            handle.snapshot(crate::SnapshotKind::Filesystem),
        );
        growth?;
        let capture = capture?;
        assert_eq!(balloon::grant(&config, &record)?, 8192);
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
        request.spec.resources = SandboxResourceShape::new(1, 8192);
        let resumed = backend.acquire(request.clone()).await?;
        let resumed_record = allocation(&backend, &request).await?;
        assert_eq!(balloon::grant(&config, &resumed_record)?, 8192);
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
        write_six_gib(&handle).await?;
        eprintln!("wrote and verified 6 GiB after growing from 4 to 8 GiB");
        Ok(())
    }
    .await;
    backend.terminate_all().await?;
    result
}
