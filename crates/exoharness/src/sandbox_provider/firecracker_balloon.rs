use super::*;

pub(super) const KVM_CAP_NR_MEMSLOTS: libc::c_int = 10;
pub(super) const KVM_CAP_MAX_VCPUS: libc::c_int = 66;

const BALLOON_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Serialize)]
pub(super) struct Configuration {
    amount_mib: u32,
    deflate_on_oom: bool,
    stats_polling_interval_s: u32,
}

#[derive(Serialize)]
pub(super) struct HotplugConfiguration {
    total_size_mib: u32,
    slot_size_mib: u32,
}

#[derive(Serialize, Deserialize)]
struct HotplugTarget {
    requested_size_mib: u32,
}

#[derive(Debug, Deserialize)]
struct HotplugStatistics {
    requested_size_mib: u32,
    plugged_size_mib: u32,
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

pub(super) fn kvm_capability(capability: libc::c_int) -> Result<u32> {
    use std::os::fd::AsRawFd;
    let kvm = File::open("/dev/kvm")?;
    // SAFETY: KVM_CHECK_EXTENSION reads an integer capability, not a pointer.
    const KVM_CHECK_EXTENSION: libc::c_ulong = 0xAE03;
    let value = unsafe { libc::ioctl(kvm.as_raw_fd(), KVM_CHECK_EXTENSION, capability) };
    ensure!(value > 0, "KVM capability {capability} is unavailable");
    Ok(u32::try_from(value)?)
}

pub(super) fn ceiling(runtime: &FirecrackerRuntimeFingerprint) -> Option<SandboxResourceShape> {
    runtime
        .growth_capacity
        .map(|capacity| capacity.resources)
        .or_else(|| SandboxResourceShape::new(runtime.vcpu_count, runtime.memory_mib))
}

pub(super) fn validate_grant(
    runtime: &FirecrackerRuntimeFingerprint,
    grant: Option<SandboxResourceShape>,
) -> Result<SandboxResourceShape> {
    ensure!(
        grant.is_some() || runtime.growth_capacity.is_none(),
        "live Firecracker state is missing its resource grant"
    );
    let grant = grant
        .or_else(|| SandboxResourceShape::new(runtime.vcpu_count, runtime.memory_mib))
        .context("invalid Firecracker resource grant")?;
    let ceiling = ceiling(runtime).context("invalid Firecracker resource capacity")?;
    runtime.validate_memory_grant(grant.memory_mib.get())?;
    ensure!(
        grant.vcpu_count.get() >= runtime.vcpu_count && grant.vcpu_count <= ceiling.vcpu_count,
        "Firecracker resource grant must grow within the running VM's capacity"
    );
    Ok(grant)
}

pub(super) fn resources(record: &MachineRecord) -> Result<SandboxResourceShape> {
    validate_grant(&record.runtime, record.resource_grant)
}

pub(super) fn grant(record: &MachineRecord) -> Result<u32> {
    Ok(resources(record)?.memory_mib.get())
}

pub(super) fn cpu_quota(vcpus: u8) -> String {
    format!("{} 100000", u64::from(vcpus) * 100_000)
}

fn cpu_ids(value: &str) -> Result<Vec<usize>> {
    let mut ids = Vec::new();
    for range in value.trim().split(',').filter(|range| !range.is_empty()) {
        let (first, last) = range.split_once('-').unwrap_or((range, range));
        let first: u16 = first.parse()?;
        let last: u16 = last.parse()?;
        ensure!(first <= last, "invalid CPU range");
        ids.extend((first..=last).map(usize::from));
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

pub(super) fn online_cpu_ids() -> Result<Vec<usize>> {
    cpu_ids(&fs::read_to_string("/sys/devices/system/cpu/online")?)
}

fn optional_controller_file(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value)),
        // Root cgroups have no controller limits; a new jailer parent may not exist yet.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn host_growth_capacity(
    config: &FirecrackerConfig,
) -> Result<FirecrackerGrowthCapacity> {
    let info = fs::read_to_string("/proc/meminfo")?;
    let kib: u64 = info
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|value| value.split_whitespace().next())
        .context("host memory capacity is unavailable")?
        .parse()?;
    let bytes = kib
        .checked_mul(1024)
        .context("host memory capacity overflow")?;
    let root = Path::new("/sys/fs/cgroup");
    let parent = root.join(
        config
            .firecracker_bin
            .file_name()
            .context("invalid Firecracker binary")?,
    );
    let max_vcpus = u8::try_from(kvm_capability(KVM_CAP_MAX_VCPUS)?.min(32))?;
    let resources = allocation_capacity(
        bytes,
        online_cpu_ids()?,
        &[root, &parent],
        max_vcpus,
        config.memory_overhead_percent,
    )?;
    let slots = kvm_capability(KVM_CAP_NR_MEMSLOTS)?
        .checked_sub(8)
        .filter(|slots| *slots > 0)
        .context("KVM has too few memory slots")?;
    Ok(FirecrackerGrowthCapacity {
        resources,
        memory_slot_size_mib: resources
            .memory_mib
            .get()
            .div_ceil(slots)
            .next_power_of_two()
            .max(128),
    })
}

fn allocation_capacity(
    mut bytes: u64,
    mut cpus: Vec<usize>,
    groups: &[&Path],
    max_vcpus: u8,
    memory_overhead_percent: u8,
) -> Result<SandboxResourceShape> {
    // Jailer places VMMs below its allocation root, outside the daemon's cgroup.
    let mut cpu_limit = u64::from(max_vcpus);
    for group in groups {
        if let Some(value) = optional_controller_file(&group.join("cpuset.cpus.effective"))? {
            let allowed = cpu_ids(&value)?;
            cpus.retain(|cpu| allowed.binary_search(cpu).is_ok());
        }
        if let Some(value) = optional_controller_file(&group.join("cpu.max"))? {
            let mut fields = value.split_whitespace();
            let quota = fields.next().context("missing CPU quota")?;
            let period: u64 = fields.next().context("missing CPU period")?.parse()?;
            ensure!(period > 0 && fields.next().is_none(), "invalid CPU quota");
            if quota != "max" {
                cpu_limit = cpu_limit.min(quota.parse::<u64>()? / period);
            }
        }
        if let Some(value) = optional_controller_file(&group.join("memory.max"))?
            && value.trim() != "max"
        {
            bytes = bytes.min(value.trim().parse()?);
        }
    }
    let cpu = u8::try_from(cpu_limit.min(cpus.len() as u64))?;
    let memory =
        u32::try_from((bytes / (1024 * 1024)) * 100 / (100 + u64::from(memory_overhead_percent)))?;
    SandboxResourceShape::new(cpu, memory).context("sandbox allocation capacity is empty")
}

pub(super) fn hotplug_configuration(
    runtime: &FirecrackerRuntimeFingerprint,
) -> Option<HotplugConfiguration> {
    let total_size_mib = runtime.hotplug_memory_mib();
    (total_size_mib > 0).then_some(HotplugConfiguration {
        total_size_mib,
        slot_size_mib: runtime
            .growth_capacity
            .map_or(128, |capacity| capacity.memory_slot_size_mib),
    })
}

pub(super) fn configuration(record: &MachineRecord) -> Result<Option<Configuration>> {
    if record.runtime.growth_capacity.is_none() {
        return Ok(None);
    }
    let grant = grant(record)?;
    Ok(Some(Configuration {
        amount_mib: record.runtime.physical_memory_mib(grant) - grant,
        deflate_on_oom: false,
        stats_polling_interval_s: 3600,
    }))
}

pub(super) fn deadline(memory_mib: u32) -> Instant {
    // Bound how long an unresponsive guest can delay other lifecycle operations.
    let seconds = (30 + u64::from(memory_mib).div_ceil(128)).min(120);
    Instant::now() + Duration::from_secs(seconds)
}

pub(super) fn api_timeout(deadline: Instant) -> Result<Duration> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .context("Firecracker resource update deadline expired")?;
    Ok(remaining.min(FIRECRACKER_API_TIMEOUT))
}

fn wait_for_target(socket: &Path, amount_mib: u32, deadline: Instant) -> Result<()> {
    let pages = u64::from(amount_mib) * 256;
    let mut observed: Option<Statistics> = None;
    loop {
        let response = api_timeout(deadline)
            .and_then(|timeout| {
                firecracker_api_response(socket, "GET", "/balloon/statistics", &[], timeout)
            })
            .with_context(|| {
                format!("Firecracker balloon readiness failed: target {pages} pages, observed {observed:?}")
            })?;
        let stats: Statistics = serde_json::from_slice(&response)?;
        if stats.target_pages == pages && stats.actual_pages == pages {
            return Ok(());
        }
        observed = Some(stats);
        std::thread::sleep(
            BALLOON_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

#[tracing::instrument(name = "firecracker.resources_ready", skip_all)]
pub(super) fn initialize_until(
    config: &FirecrackerConfig,
    record: &MachineRecord,
    deadline: Instant,
) -> Result<()> {
    let grant = grant(record)?;
    let runtime = &record.runtime;
    ensure!(
        runtime.growth_capacity.is_some(),
        "Firecracker has no live memory capacity"
    );
    let socket = jail_root(config, &record.machine_id).join("run/firecracker.socket");
    if runtime.hotplug_memory_mib() > 0 {
        let requested_size_mib = runtime.physical_memory_mib(grant) - runtime.boot_memory_mib();
        firecracker_api_request(
            &socket,
            "PATCH",
            "/hotplug/memory",
            &HotplugTarget { requested_size_mib },
            api_timeout(deadline)?,
        )?;
        let mut observed: Option<HotplugStatistics> = None;
        loop {
            let timeout = api_timeout(deadline).with_context(|| format!(
                "Firecracker memory hotplug timed out: target {requested_size_mib} MiB, observed {observed:?}; verify CONFIG_VIRTIO_MEM and automatic memory onlining"
            ))?;
            let response =
                firecracker_api_response(&socket, "GET", "/hotplug/memory", &[], timeout)?;
            let stats: HotplugStatistics = serde_json::from_slice(&response)?;
            if stats.requested_size_mib == requested_size_mib
                && stats.plugged_size_mib == requested_size_mib
            {
                break;
            }
            observed = Some(stats);
            std::thread::sleep(
                BALLOON_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    let target = runtime.physical_memory_mib(grant) - grant;
    firecracker_api_request(
        &socket,
        "PATCH",
        "/balloon",
        &BalloonTarget { amount_mib: target },
        api_timeout(deadline)?,
    )?;
    wait_for_target(&socket, target, deadline)
}

pub(super) fn expand(
    config: &FirecrackerConfig,
    record: &MachineRecord,
    target: SandboxResourceShape,
    host_capacity: SandboxResourceShape,
    deadline: Instant,
) -> crate::SandboxResult<crate::SandboxResourceUpdate> {
    let current = resources(record)?;
    let ceiling = ceiling(&record.runtime).context("invalid Firecracker resource capacity")?;
    if record.runtime.growth_capacity.is_none()
        || target.vcpu_count < current.vcpu_count
        || target.memory_mib < current.memory_mib
        || target.vcpu_count > ceiling.vcpu_count
        || target.memory_mib > ceiling.memory_mib
        || (target.vcpu_count > current.vcpu_count && target.vcpu_count > host_capacity.vcpu_count)
        || (target.memory_mib > current.memory_mib && target.memory_mib > host_capacity.memory_mib)
    {
        return Err(crate::SandboxFailure::InvalidRequest {
            message: "Firecracker live expansion must grow within the running VM's capacity".into(),
        }
        .into());
    }
    if !process_running(&jail_root(config, &record.machine_id).join("firecracker.pid")) {
        return Err(anyhow!("Firecracker expansion source is not running").into());
    }
    apply_grant(
        config,
        record,
        &firecracker_cgroup_dir(config, &record.machine_id),
        target,
        deadline,
    )
}

pub(super) fn apply_host_memory_limits(
    config: &FirecrackerConfig,
    cgroup: &Path,
    resources: SandboxResourceShape,
) -> Result<()> {
    let memory_max = config.memory_footprint_mib(resources.memory_mib.get()) * 1024 * 1024;
    fs::write(cgroup.join("memory.max"), memory_max.to_string())?;
    fs::write(cgroup.join("memory.high"), "max")?;
    Ok(())
}

fn apply_grant(
    config: &FirecrackerConfig,
    record: &MachineRecord,
    cgroup: &Path,
    target: SandboxResourceShape,
    deadline: Instant,
) -> crate::SandboxResult<crate::SandboxResourceUpdate> {
    let mut record = record.clone();
    record.resource_grant = Some(target);
    // Journal the reservation before changing any host or guest limit.
    let applied = (|| -> Result<()> {
        replace_manifest(&config.state_root, &record)?;
        apply_host_memory_limits(config, cgroup, target)?;
        fs::write(cgroup.join("cpu.max"), cpu_quota(target.vcpu_count.get()))?;
        initialize_until(config, &record, deadline)
    })();
    Ok(match applied {
        Ok(()) => crate::SandboxResourceUpdate::Applied { grant: target },
        Err(error) => crate::SandboxResourceUpdate::Incomplete {
            reservation: target,
            error: error.into(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_capacity_honors_jailer_parent_limits_and_cpu_ids() -> Result<()> {
        let root = tempfile::tempdir()?;
        let parent = root.path().join("firecracker");
        fs::create_dir(&parent)?;
        fs::write(root.path().join("cpuset.cpus.effective"), "0-7")?;
        fs::write(root.path().join("cpu.max"), "600000 100000")?;
        fs::write(parent.join("cpuset.cpus.effective"), "2-5")?;
        fs::write(parent.join("cpu.max"), "250000 100000")?;
        fs::write(
            parent.join("memory.max"),
            (6_u64 * 1024 * 1024 * 1024).to_string(),
        )?;
        let groups = [root.path(), parent.as_path()];
        let physical = 16_u64 * 1024 * 1024 * 1024;
        assert_eq!(
            allocation_capacity(physical, (0..8).collect(), &groups, 32, 20)?,
            SandboxResourceShape::new(2, 5120).unwrap()
        );
        fs::write(parent.join("cpu.max"), "max 100000")?;
        assert_eq!(
            allocation_capacity(physical, (0..8).collect(), &groups, 32, 20)?,
            SandboxResourceShape::new(4, 5120).unwrap()
        );
        fs::write(parent.join("cpuset.cpus.effective"), "8-9")?;
        assert!(allocation_capacity(physical, (0..8).collect(), &groups, 32, 20).is_err());
        Ok(())
    }

    fn record() -> MachineRecord {
        let mut runtime = super::super::tests::test_runtime();
        runtime.vcpu_count = 2;
        runtime.memory_mib = 4096;
        runtime.growth_capacity = Some(super::super::tests::test_growth_capacity(8, 16384));
        MachineRecord {
            machine_id: "fc-0123456789abcdef-01234567".to_owned(),
            spec_hash: "spec".to_owned(),
            runtime,
            resource_grant: SandboxResourceShape::new(2, 4096),
            external_disks: false,
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
    fn journal_write_failure_returns_an_incomplete_reservation() -> Result<()> {
        let root = tempfile::tempdir()?;
        let config = FirecrackerConfig {
            state_root: root.path().into(),
            ..Default::default()
        };
        let target = SandboxResourceShape::new(2, 8192).unwrap();
        let update = apply_grant(
            &config,
            &record(),
            root.path(),
            target,
            deadline(target.memory_mib.get()),
        )?;
        assert!(
            matches!(update, crate::SandboxResourceUpdate::Incomplete { reservation, .. } if reservation == target)
        );
        Ok(())
    }

    #[test]
    fn timeout_reports_observed_balloon_pages() -> Result<()> {
        let root = tempfile::tempdir_in("/tmp")?;
        let socket = root.path().join("api");
        let listener = StdUnixListener::bind(&socket)?;
        let (release, blocked) = std::sync::mpsc::channel();
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
            blocked.recv()?;
            Ok(())
        });
        let error =
            wait_for_target(&socket, 1, Instant::now() + Duration::from_secs(2)).unwrap_err();
        release.send(())?;
        peer.join().unwrap()?;
        assert!(error.to_string().contains("actual_pages: 7"), "{error:#}");
        Ok(())
    }

    #[test]
    fn api_waits_share_the_remaining_resize_deadline() -> Result<()> {
        let root = tempfile::tempdir_in("/tmp")?;
        let config = FirecrackerConfig {
            state_root: root.path().into(),
            ..Default::default()
        };
        let record = record();
        let socket = jail_root(&config, &record.machine_id).join("run/firecracker.socket");
        fs::create_dir_all(socket.parent().context("socket parent")?)?;
        let listener = StdUnixListener::bind(socket)?;
        let (release, blocked) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || -> Result<()> {
            let (mut stream, _) = listener.accept()?;
            let (request, _) = super::super::tests::read_api_request(&mut stream)?;
            assert!(request.starts_with("PATCH /hotplug/memory "));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")?;
            let (mut stream, _) = listener.accept()?;
            let (request, _) = super::super::tests::read_api_request(&mut stream)?;
            assert!(request.starts_with("GET /hotplug/memory "));
            blocked.recv()?;
            Ok(())
        });
        let started = Instant::now();
        let error =
            initialize_until(&config, &record, started + Duration::from_secs(2)).unwrap_err();
        let elapsed = started.elapsed();
        release.send(())?;
        peer.join().unwrap()?;
        assert!(
            error.chain().any(|cause| cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)),
            "{error:#}"
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "API wait ignored the remaining deadline: {elapsed:?}"
        );
        Ok(())
    }

    #[test]
    fn failed_host_limit_retains_the_reservation_for_retry() -> Result<()> {
        let root = tempfile::tempdir()?;
        let config = FirecrackerConfig {
            state_root: root.path().into(),
            ..Default::default()
        };
        let record = record();
        fs::create_dir_all(config.state_root.join("manifests"))?;
        write_manifest(&config.state_root, &record)?;
        let before = fs::read(manifest_path(&config.state_root, &record.machine_id))?;
        let target = SandboxResourceShape::new(2, 8192).unwrap();
        let update = apply_grant(
            &config,
            &record,
            &root.path().join("missing-cgroup"),
            target,
            deadline(target.memory_mib.get()),
        )
        .unwrap();
        assert!(
            matches!(update, crate::SandboxResourceUpdate::Incomplete { reservation, .. } if reservation == target)
        );
        let after = fs::read(manifest_path(&config.state_root, &record.machine_id))?;
        assert_ne!(after, before);
        let persisted: MachineRecord = serde_json::from_slice(&after)?;
        assert_eq!(grant(&persisted)?, 8192);
        Ok(())
    }

    #[test]
    fn host_limit_precedes_deflation_and_completion_waits_for_actual_pages() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let config = FirecrackerConfig {
            state_root: root.path().to_owned(),
            memory_overhead_percent: 25,
            ..Default::default()
        };
        let record = record();
        fs::create_dir_all(config.state_root.join("manifests")).unwrap();
        write_manifest(&config.state_root, &record).unwrap();
        let initial = configuration(&record).unwrap().unwrap();
        assert_eq!(initial.amount_mib, 0);
        assert!(!initial.deflate_on_oom);
        let resources = SandboxResourceShape::new(4, 8192).unwrap();
        let cgroup = root.path().join("cgroup");
        fs::create_dir(&cgroup).unwrap();
        fs::create_dir(cgroup.join("cpu.max")).unwrap();
        let update = apply_grant(
            &config,
            &record,
            &cgroup,
            resources,
            deadline(resources.memory_mib.get()),
        )
        .unwrap();
        assert!(matches!(update, crate::SandboxResourceUpdate::Incomplete {
            reservation,
            error: crate::SandboxError::Internal(error),
        } if reservation == resources && error.downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.raw_os_error() == Some(libc::EISDIR))));
        let record: MachineRecord = serde_json::from_slice(
            &fs::read(manifest_path(&config.state_root, &record.machine_id)).unwrap(),
        )
        .unwrap();
        assert_eq!(record.resource_grant, Some(resources));
        fs::remove_dir(cgroup.join("cpu.max")).unwrap();
        let socket = jail_root(&config, &record.machine_id).join("run/firecracker.socket");
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = StdUnixListener::bind(&socket).unwrap();
        let cgroup_for_peer = cgroup.clone();
        let peer = std::thread::spawn(move || {
            let replies = [
                ("PATCH /hotplug/memory ", 500, ""),
                ("PATCH /hotplug/memory ", 200, ""),
                (
                    "GET /hotplug/memory ",
                    200,
                    r#"{"requested_size_mib":4096,"plugged_size_mib":2048}"#,
                ),
                (
                    "GET /hotplug/memory ",
                    200,
                    r#"{"requested_size_mib":4096,"plugged_size_mib":4096}"#,
                ),
                ("PATCH /balloon ", 200, ""),
                (
                    "GET /balloon/statistics ",
                    200,
                    r#"{"target_pages":0,"actual_pages":1}"#,
                ),
                (
                    "GET /balloon/statistics ",
                    200,
                    r#"{"target_pages":0,"actual_pages":0}"#,
                ),
            ];
            for (path, status, response) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let (request, body) = super::super::tests::read_api_request(&mut stream).unwrap();
                assert!(request.starts_with(path), "{request}");
                assert_eq!(
                    fs::read_to_string(cgroup_for_peer.join("cpu.max")).unwrap(),
                    cpu_quota(4)
                );
                assert_eq!(
                    fs::read_to_string(cgroup_for_peer.join("memory.max")).unwrap(),
                    (10240_u64 * 1024 * 1024).to_string()
                );
                assert_eq!(
                    fs::read_to_string(cgroup_for_peer.join("memory.high")).unwrap(),
                    "max"
                );
                if path == "PATCH /hotplug/memory " {
                    assert_eq!(
                        serde_json::from_slice::<HotplugTarget>(&body)
                            .unwrap()
                            .requested_size_mib,
                        4096
                    );
                } else if path == "PATCH /balloon " {
                    assert_eq!(
                        serde_json::from_slice::<BalloonTarget>(&body)
                            .unwrap()
                            .amount_mib,
                        0
                    );
                } else {
                    assert!(body.is_empty());
                }
                write!(stream, "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            }
        });
        let trace = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW)
            .with_writer(Arc::new(trace.reopen().unwrap()))
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        assert!(
            matches!(apply_grant(&config, &record, &cgroup, resources, deadline(resources.memory_mib.get())).unwrap(), crate::SandboxResourceUpdate::Incomplete { reservation, .. } if reservation == resources)
        );
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
        let update = apply_grant(
            &config,
            &record,
            &cgroup,
            resources,
            deadline(resources.memory_mib.get()),
        )
        .unwrap();
        assert!(
            matches!(&update,
            crate::SandboxResourceUpdate::Applied { grant } if *grant == resources),
            "{update:?}"
        );
        peer.join().unwrap();
        let trace = fs::read_to_string(trace.path()).unwrap();
        assert!(trace.contains("firecracker.resources_ready"), "{trace}");
        assert_eq!(trace.matches("firecracker.api").count(), 3, "{trace}");
        assert!(!trace.contains("method=\"GET\""), "{trace}");
        assert_eq!(grant(&record).unwrap(), 8192);
        assert_eq!(configuration(&record).unwrap().unwrap().amount_mib, 0);
        assert!(
            expand(
                &config,
                &record,
                SandboxResourceShape::new(3, 8192).unwrap(),
                ceiling(&record.runtime).unwrap(),
                deadline(8192)
            )
            .is_err()
        );
        assert!(
            expand(
                &config,
                &record,
                SandboxResourceShape::new(2, 4096).unwrap(),
                ceiling(&record.runtime).unwrap(),
                deadline(4096)
            )
            .is_err()
        );
        assert!(
            expand(
                &config,
                &record,
                SandboxResourceShape::new(2, 32768).unwrap(),
                ceiling(&record.runtime).unwrap(),
                deadline(32768)
            )
            .is_err()
        );
    }
}
