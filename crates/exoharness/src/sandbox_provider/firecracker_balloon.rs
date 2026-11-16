use super::*;

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

#[derive(Debug, Deserialize)]
pub(super) struct Statistics {
    pub(super) target_pages: u64,
    pub(super) actual_pages: u64,
}

pub(super) fn grant(record: &MachineRecord) -> Result<u32> {
    let grant = record
        .memory_grant_mib
        .map_or(record.runtime.memory_mib, std::num::NonZeroU32::get);
    record.runtime.validate_memory_grant(grant)?;
    Ok(grant)
}

pub(super) fn configuration(record: &MachineRecord) -> Result<Option<Configuration>> {
    record
        .runtime
        .memory_ceiling_mib
        .map(|ceiling| {
            Ok(Configuration {
                amount_mib: ceiling - grant(record)?,
                deflate_on_oom: false,
                stats_polling_interval_s: 3600,
            })
        })
        .transpose()
}

fn wait_for_target(socket: &Path, amount_mib: u32, timeout: Duration) -> Result<()> {
    let pages = u64::from(amount_mib) * 256;
    let deadline = Instant::now() + timeout;
    loop {
        let response = firecracker_api_response(
            socket,
            "GET",
            "/balloon/statistics",
            &[],
            FIRECRACKER_API_TIMEOUT,
        )?;
        let stats: Statistics = serde_json::from_slice(&response)?;
        if stats.target_pages == pages && stats.actual_pages == pages {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "Firecracker balloon timed out after {timeout:?}: target {pages} pages, observed target {} and actual {} pages; guest may be slow or unable to reclaim memory",
            stats.target_pages,
            stats.actual_pages
        );
        std::thread::sleep(BALLOON_POLL_INTERVAL);
    }
}

pub(super) fn initialize(config: &FirecrackerConfig, record: &MachineRecord) -> Result<()> {
    let ceiling = record
        .runtime
        .memory_ceiling_mib
        .context("Firecracker has no balloon")?;
    let target = ceiling - grant(record)?;
    let socket = jail_root(config, &record.machine_id).join("run/firecracker.socket");
    firecracker_api_patch(&socket, "/balloon", &BalloonTarget { amount_mib: target })?;
    wait_for_target(
        &socket,
        target,
        Duration::from_secs(30 + u64::from(ceiling).div_ceil(128)),
    )
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
        memory >= grant(record)? && memory <= ceiling,
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
    // Prepare host limits before committing the larger grant.
    fs::write(
        cgroup.join("memory.max"),
        ((u64::from(memory) + FIRECRACKER_MEMORY_HEADROOM_MIB) * 1024 * 1024).to_string(),
    )?;
    fs::write(
        cgroup.join("memory.high"),
        ((u64::from(memory) + 256) * 1024 * 1024).to_string(),
    )?;
    let mut record = record.clone();
    record.memory_grant_mib = Some(resources.memory_mib);
    replace_manifest(&config.state_root, &record)?;
    // Persist intent before deflation: a lost API response can be retried, but
    // must never let another caller reduce memory the guest may already use.
    initialize(config, &record)?;
    Ok(resources)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> MachineRecord {
        let mut runtime = super::super::tests::test_runtime();
        runtime.vcpu_count = 2;
        runtime.memory_mib = 4096;
        runtime.memory_ceiling_mib = Some(16384);
        MachineRecord {
            machine_id: "fc-0123456789abcdef-01234567".to_owned(),
            spec_hash: "spec".to_owned(),
            runtime,
            memory_grant_mib: None,
            resolved_image: "image".to_owned(),
            slot: 1,
            network_enabled: false,
            workspace_id: None,
            resource_paths: Vec::new(),
            idle_ttl_seconds: None,
            snapshot_template: None,
            snapshot_network_slot: None,
        }
    }

    #[test]
    fn timeout_reports_observed_balloon_pages() -> Result<()> {
        let root = tempfile::tempdir_in("/tmp")?;
        let socket = root.path().join("api");
        let listener = StdUnixListener::bind(&socket)?;
        let peer = std::thread::spawn(move || -> Result<()> {
            let (mut stream, _) = listener.accept()?;
            let (request, body) = super::super::tests::read_api_request(&mut stream)?;
            assert!(request.starts_with("GET /balloon/statistics "));
            assert!(body.is_empty());
            let body = r#"{"target_pages":256,"actual_pages":7}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )?;
            Ok(())
        });
        let error = wait_for_target(&socket, 1, Duration::ZERO).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("target 256 pages, observed target 256 and actual 7 pages")
        );
        peer.join().unwrap()?;
        Ok(())
    }

    #[test]
    fn failed_host_limit_does_not_publish_a_grant() -> Result<()> {
        let root = tempfile::tempdir()?;
        let config = FirecrackerConfig {
            state_root: root.path().into(),
            ..Default::default()
        };
        let record = record();
        fs::create_dir_all(config.state_root.join("manifests"))?;
        write_manifest(&config.state_root, &record)?;
        let before = fs::read(manifest_path(&config.state_root, &record.machine_id))?;
        let error = apply_grant(
            &config,
            &record,
            &root.path().join("missing-cgroup"),
            SandboxResourceShape::new(2, 8192).unwrap(),
        )
        .unwrap_err();
        assert!(error.downcast_ref::<std::io::Error>().is_some());
        let after = fs::read(manifest_path(&config.state_root, &record.machine_id))?;
        assert_eq!(after, before);
        let persisted: MachineRecord = serde_json::from_slice(&after)?;
        assert_eq!(grant(&persisted)?, 4096);
        Ok(())
    }

    #[test]
    fn host_limit_precedes_deflation_and_completion_waits_for_actual_pages() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let config = FirecrackerConfig {
            state_root: root.path().to_owned(),
            ..Default::default()
        };
        let record = record();
        fs::create_dir_all(config.state_root.join("manifests")).unwrap();
        write_manifest(&config.state_root, &record).unwrap();
        let initial = configuration(&record).unwrap().unwrap();
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
                let (request, body) = super::super::tests::read_api_request(&mut stream).unwrap();
                assert!(request.starts_with(if index < 2 {
                    "PATCH /balloon "
                } else {
                    "GET /balloon/statistics "
                }));
                if index < 2 {
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
                    "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    if index == 0 {
                        "500 Internal Server Error"
                    } else {
                        "200 OK"
                    },
                    response.len(),
                    response
                )
                .unwrap();
            }
        });
        let resources = SandboxResourceShape::new(2, 8192).unwrap();
        assert!(apply_grant(&config, &record, &cgroup, resources).is_err());
        let record: MachineRecord = serde_json::from_slice(
            &fs::read(manifest_path(&config.state_root, &record.machine_id)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            fs::metadata(manifest_path(&config.state_root, &record.machine_id))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(record.runtime.memory_mib, 4096);
        assert_eq!(grant(&record).unwrap(), 8192);
        assert_eq!(
            apply_grant(&config, &record, &cgroup, resources).unwrap(),
            resources
        );
        peer.join().unwrap();
        assert_eq!(grant(&record).unwrap(), 8192);
        assert_eq!(configuration(&record).unwrap().unwrap().amount_mib, 8192);
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
