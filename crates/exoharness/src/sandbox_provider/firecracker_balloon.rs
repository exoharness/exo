use super::*;

const BALLOON_TIMEOUT: Duration = Duration::from_secs(30);
const BALLOON_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Serialize)]
pub(super) struct Configuration {
    amount_mib: u32,
    deflate_on_oom: bool,
    stats_polling_interval_s: u32,
}

#[derive(Serialize, Deserialize)]
struct BalloonTarget {
    amount_mib: u32,
}

#[derive(Deserialize)]
struct BalloonStatistics {
    target_pages: u64,
    actual_pages: u64,
}

pub(super) fn grant(config: &FirecrackerConfig, record: &MachineRecord) -> Result<u32> {
    let path = jail_dir(config, &record.machine_id).join("memory-grant");
    let grant = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<u32>(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => record.runtime.memory_mib,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        grant >= record.runtime.memory_mib
            && grant
                <= record
                    .runtime
                    .memory_ceiling_mib
                    .unwrap_or(record.runtime.memory_mib),
        "invalid Firecracker memory grant"
    );
    Ok(grant)
}

pub(super) fn configuration(
    config: &FirecrackerConfig,
    record: &MachineRecord,
) -> Result<Option<Configuration>> {
    record
        .runtime
        .memory_ceiling_mib
        .map(|ceiling| {
            Ok(Configuration {
                amount_mib: ceiling - grant(config, record)?,
                deflate_on_oom: false,
                stats_polling_interval_s: 1,
            })
        })
        .transpose()
}

fn wait_for_target(socket: &Path, amount_mib: u32) -> Result<()> {
    let pages = u64::from(amount_mib) * 256;
    let deadline = Instant::now() + BALLOON_TIMEOUT;
    loop {
        let response = firecracker_api_response(
            socket,
            "GET",
            "/balloon/statistics",
            &(),
            FIRECRACKER_API_TIMEOUT,
        )?;
        let stats: BalloonStatistics = serde_json::from_slice(&response)?;
        if stats.target_pages == pages && stats.actual_pages == pages {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "Firecracker balloon did not reach its target; verify CONFIG_VIRTIO_BALLOON kernel support"
        );
        std::thread::sleep(BALLOON_POLL_INTERVAL);
    }
}

pub(super) fn initialize(config: &FirecrackerConfig, record: &MachineRecord) -> Result<()> {
    let ceiling = record
        .runtime
        .memory_ceiling_mib
        .context("Firecracker has no balloon")?;
    let target = ceiling - grant(config, record)?;
    let socket = jail_root(config, &record.machine_id).join("run/firecracker.socket");
    firecracker_api_patch(&socket, "/balloon", &BalloonTarget { amount_mib: target })?;
    wait_for_target(&socket, target)
}

pub(super) fn expand(
    config: &FirecrackerConfig,
    record: &MachineRecord,
    resources: SandboxResourceShape,
) -> Result<SandboxResourceShape> {
    let ceiling = record
        .runtime
        .memory_ceiling_mib
        .context("Firecracker has no live memory ceiling")?;
    let memory = resources.memory_mib.get();
    ensure!(
        resources.vcpu_count.get() == record.runtime.vcpu_count,
        "Firecracker live expansion cannot change vCPU count"
    );
    ensure!(
        memory >= grant(config, record)? && memory <= ceiling,
        "Firecracker live expansion must grow within the boot memory ceiling"
    );
    ensure!(
        process_running(&jail_root(config, &record.machine_id).join("firecracker.pid")),
        "Firecracker expansion source is not running"
    );
    apply_grant(
        config,
        record,
        &firecracker_cgroup_dir(config, &record.machine_id),
        resources,
    )
}

fn apply_grant(
    config: &FirecrackerConfig,
    record: &MachineRecord,
    cgroup: &Path,
    resources: SandboxResourceShape,
) -> Result<SandboxResourceShape> {
    let memory = resources.memory_mib.get();
    let directory = jail_dir(config, &record.machine_id);
    let temporary = directory.join("memory-grant.tmp");
    let mut file = File::create(&temporary)?;
    fs::set_permissions(&temporary, Permissions::from_mode(0o600))?;
    file.write_all(&serde_json::to_vec(&memory)?)?;
    file.sync_all()?;
    fs::rename(&temporary, directory.join("memory-grant"))?;
    File::open(&directory)?.sync_all()?;
    fs::write(
        cgroup.join("memory.max"),
        ((u64::from(memory) + 1024) * 1024 * 1024).to_string(),
    )?;
    fs::write(
        cgroup.join("memory.high"),
        ((u64::from(memory) + 256) * 1024 * 1024).to_string(),
    )?;
    initialize(config, record)?;
    Ok(resources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_limit_precedes_deflation_and_completion_waits_for_actual_pages() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let config = FirecrackerConfig {
            state_root: root.path().to_owned(),
            ..Default::default()
        };
        let mut runtime = super::super::tests::test_runtime();
        runtime.vcpu_count = 2;
        runtime.memory_mib = 4096;
        runtime.memory_ceiling_mib = Some(16384);
        let record = MachineRecord {
            machine_id: "fc-0123456789abcdef-01234567".to_owned(),
            spec_hash: "spec".to_owned(),
            runtime,
            resolved_image: "image".to_owned(),
            slot: 1,
            network_enabled: false,
            workspace_id: None,
            resource_paths: Vec::new(),
            idle_ttl_seconds: None,
            snapshot_template: None,
            snapshot_network_slot: None,
        };
        let initial = configuration(&config, &record).unwrap().unwrap();
        assert_eq!(initial.amount_mib, 12288);
        assert!(!initial.deflate_on_oom);
        let socket = jail_root(&config, &record.machine_id).join("run/firecracker.socket");
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = StdUnixListener::bind(&socket).unwrap();
        let cgroup = root.path().join("cgroup");
        fs::create_dir(&cgroup).unwrap();
        let cgroup_for_peer = cgroup.clone();
        let peer = std::thread::spawn(move || {
            for (index, response) in [
                String::new(),
                serde_json::json!({"target_pages": 8192 * 256, "actual_pages": 12288 * 256})
                    .to_string(),
                serde_json::json!({"target_pages": 8192 * 256, "actual_pages": 8192 * 256 + 1})
                    .to_string(),
                serde_json::json!({"target_pages": 8192 * 256, "actual_pages": 8192 * 256})
                    .to_string(),
            ]
            .into_iter()
            .enumerate()
            {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = StdBufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                assert!(request.starts_with(if index == 0 {
                    "PATCH /balloon "
                } else {
                    "GET /balloon/statistics "
                }));
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header == "\r\n" {
                        break;
                    }
                    if let Some(value) = header.strip_prefix("Content-Length: ") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                if index == 0 {
                    assert_eq!(
                        serde_json::from_slice::<BalloonTarget>(&body)
                            .unwrap()
                            .amount_mib,
                        8192
                    );
                    assert_eq!(
                        fs::read_to_string(cgroup_for_peer.join("memory.max")).unwrap(),
                        ((8192_u64 + 1024) * 1024 * 1024).to_string()
                    );
                    assert_eq!(
                        fs::read_to_string(cgroup_for_peer.join("memory.high")).unwrap(),
                        ((8192_u64 + 256) * 1024 * 1024).to_string()
                    );
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                )
                .unwrap();
            }
        });
        let resources = SandboxResourceShape::new(2, 8192).unwrap();
        assert_eq!(
            apply_grant(&config, &record, &cgroup, resources).unwrap(),
            resources
        );
        peer.join().unwrap();
        assert_eq!(grant(&config, &record).unwrap(), 8192);
        assert_eq!(
            configuration(&config, &record).unwrap().unwrap().amount_mib,
            8192
        );
        assert!(
            expand(
                &config,
                &record,
                SandboxResourceShape::new(3, 8192).unwrap()
            )
            .is_err()
        );
        assert!(
            expand(
                &config,
                &record,
                SandboxResourceShape::new(2, 4096).unwrap()
            )
            .is_err()
        );
        assert!(
            expand(
                &config,
                &record,
                SandboxResourceShape::new(2, 32768).unwrap()
            )
            .is_err()
        );
    }
}
