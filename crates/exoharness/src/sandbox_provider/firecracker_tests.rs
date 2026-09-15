use super::*;
use crate::egress::{EgressTransport, LocalEgressTransport};
use tokio::net::UnixListener;

fn test_host_runtime() -> FirecrackerHostFingerprint {
    FirecrackerHostFingerprint {
        architecture: "x86_64".to_string(),
        protocol_version: PROTOCOL_VERSION,
        firecracker_version: "v1.16.1".to_string(),
        firecracker_sha256: "firecracker".to_string(),
        jailer_sha256: "jailer".to_string(),
        kernel_sha256: "kernel".to_string(),
        initramfs_sha256: "initramfs".to_string(),
        network_device_policy: FirecrackerNetworkDevicePolicy::default(),
    }
}

fn test_runtime() -> FirecrackerRuntimeFingerprint {
    test_host_runtime().for_resources(SandboxResourceShape::default())
}

#[test]
fn runtime_fingerprint_uses_requested_resources() {
    let resources = SandboxResourceShape::new(8, 16_384).unwrap();
    let runtime = test_host_runtime().for_resources(resources);
    assert_eq!(runtime.vcpu_count, 8);
    assert_eq!(runtime.memory_mib, 16_384);
}

#[test]
fn firecracker_validates_provider_specific_resource_limits() {
    validate_resource_shape(SandboxResourceShape::new(1, 128).unwrap()).unwrap();
    assert!(validate_resource_shape(SandboxResourceShape::new(33, 4096).unwrap()).is_err());
    assert!(validate_resource_shape(SandboxResourceShape::new(1, 127).unwrap()).is_err());
}

#[test]
fn network_device_policy_can_keep_disabled_sandboxes_host_reachable() {
    let mut config = FirecrackerConfig::default();
    assert!(network_device_enabled(
        &config,
        &SandboxNetworkPolicy::Unrestricted
    ));
    assert!(!network_device_enabled(
        &config,
        &SandboxNetworkPolicy::Disabled
    ));

    config.network_device_policy = FirecrackerNetworkDevicePolicy::AllSandboxes;
    assert!(network_device_enabled(
        &config,
        &SandboxNetworkPolicy::Unrestricted
    ));
    assert!(network_device_enabled(
        &config,
        &SandboxNetworkPolicy::Disabled
    ));
}

#[test]
fn disabled_sandbox_network_rejects_unconfigured_egress() {
    let config = FirecrackerConfig {
        allowed_egress_cidrs: vec!["192.0.2.0/24".parse().unwrap()],
        ..Default::default()
    };
    let network = network_config(1);
    let rules =
        network_firewall_rules(&config, &network, &SandboxNetworkPolicy::Disabled, None).unwrap();

    assert!(rules.contains("ip daddr 192.0.2.0/24 counter accept"));
    assert!(rules.contains(&format!(
        "forward iifname {} counter reject",
        network.host_veth
    )));
    assert!(!rules.contains(&format!(
        "forward iifname {} counter accept\n",
        network.host_veth
    )));
}

#[test]
fn enabled_sandbox_network_accepts_public_egress() {
    let config = FirecrackerConfig::default();
    let network = network_config(1);
    let rules =
        network_firewall_rules(&config, &network, &SandboxNetworkPolicy::Unrestricted, None)
            .unwrap();

    assert!(rules.contains(&format!(
        "forward iifname {} counter accept\n",
        network.host_veth
    )));
}

#[test]
fn proxy_transport_enforces_egress_independently_of_network_policy() {
    let config = FirecrackerConfig {
        allowed_egress_cidrs: vec!["0.0.0.0/0".parse().unwrap()],
        ..Default::default()
    };
    let network = network_config(1);
    let proxy = crate::SandboxEgressProxy {
        http: "192.0.2.10:18080".parse().unwrap(),
        https: "192.0.2.10:18443".parse().unwrap(),
        dns: "192.0.2.10:1053".parse().unwrap(),
    };
    let limited = SandboxNetworkPolicy::Limited {
        allowed_hosts: vec!["api.notion.com".into()],
    };
    let request = |networking, egress_proxy| FirecrackerRequest {
        sandbox: SandboxRequest {
            sandbox_id: "policy-validation".into(),
            scope: None,
            provider_state: None,
            spec: SandboxSpec {
                image: String::new(),
                resources: Default::default(),
                mounts: vec![],
                durable_file_systems: vec![],
                default_workdir: "/home/exo/workspace".into(),
                policy: crate::EgressPolicy::from(networking),
            },
            lifecycle: Default::default(),
        },
        egress_proxy,
    };
    assert!(prepare_request(request(limited.clone(), None)).is_err());
    assert!(prepare_request(request(SandboxNetworkPolicy::Disabled, Some(proxy))).is_err());
    for policy in [limited, SandboxNetworkPolicy::Unrestricted] {
        let rules = network_firewall_rules(&config, &network, &policy, Some(proxy)).unwrap();
        assert!(rules.contains("tcp dport 443 counter dnat ip to 192.0.2.10:18443"));
        assert!(rules.contains(&format!(
            "forward iifname {} counter reject",
            network.host_veth
        )));
        assert!(!rules.contains("ip daddr 0.0.0.0/0 counter accept"));
    }
}

#[test]
fn sparse_copy_modes_never_allow_automatic_reflink_fallback() {
    assert_eq!(
        SparseCopyMode::Full.cp_args(),
        ["--sparse=always", "--reflink=never"]
    );
    assert_eq!(
        SparseCopyMode::ReflinkRequired.cp_args(),
        ["--sparse=auto", "--reflink=always"]
    );
}

#[test]
fn resource_names_and_addresses_are_distinct() {
    let first = network_config(1);
    let second = network_config(2);
    assert_ne!(first.namespace, second.namespace);
    assert_ne!(first.host_veth, second.host_veth);
    assert_ne!(first.nft_table, second.nft_table);
    assert_ne!(first.guest_ip, second.guest_ip);
    assert_ne!(first.guest_cidr, second.guest_cidr);
}

#[test]
fn validates_machine_ids() {
    assert!(valid_machine_id("fc-0123456789abcdef-01234567"));
    assert!(!valid_machine_id("../firecracker"));
    let one_shot = one_shot_machine_id("sandbox", "0123456789abcdef", u64::MAX);
    assert!(valid_machine_id(&one_shot));
    assert_eq!(one_shot.len(), MAX_MACHINE_ID.len());
}

#[tokio::test]
async fn lifecycle_locks_serialize_a_machine_family_but_not_other_machines() {
    let locks = MachineLifecycleLocks::default();
    let first = "fc-0123456789abcdef-aaaaaaaa";
    let same_sandbox_new_spec = "fc-0123456789abcdef-bbbbbbbb";
    let other = "fc-fedcba9876543210-aaaaaaaa";

    let first_guard = locks.lock_machine(first).await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            locks.lock_machine(same_sandbox_new_spec)
        )
        .await
        .is_err()
    );
    let other_guard = tokio::time::timeout(Duration::from_millis(20), locks.lock_machine(other))
        .await
        .expect("an unrelated machine lifecycle must not be serialized");
    drop(other_guard);
    drop(first_guard);

    tokio::time::timeout(
        Duration::from_millis(20),
        locks.lock_machine(same_sandbox_new_spec),
    )
    .await
    .expect("the machine-family lock must be released with its guard");

    let key = "sandbox";
    let machine_id = machine_id(key, "0123456789abcdef");
    let sandbox_guard = locks.lock_sandbox(key).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), locks.lock_machine(&machine_id))
            .await
            .is_err()
    );
    drop(sandbox_guard);

    let other_key = "other";
    let (first_pair_guard, second_pair_guard) = locks.lock_sandbox_pair(key, other_key).await;
    assert!(second_pair_guard.is_some());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), locks.lock_sandbox(key))
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), locks.lock_sandbox(other_key))
            .await
            .is_err()
    );
    drop(first_pair_guard);
    drop(second_pair_guard);
}

// Caught live: growing machine ids from 16 to 32 hash characters pushed
// the jailed API socket past sun_path's 108 bytes and made the backend
// reject the README's default state root outright.
#[test]
fn default_state_root_fits_all_jailed_socket_paths() {
    validate_jailed_socket_paths(&FirecrackerConfig::default())
        .expect("default state root must fit the jailed socket path budget");
}

#[test]
fn validates_ext4_magic() {
    let directory = tempfile::tempdir().unwrap();
    let image_path = directory.path().join("rootfs.ext4");
    let mut image = File::create(&image_path).unwrap();
    image.set_len(2048).unwrap();
    image.seek(SeekFrom::Start(1024 + 0x38)).unwrap();
    image.write_all(&[0x53, 0xef]).unwrap();
    image.flush().unwrap();
    assert!(validate_ext4_image(&image_path).is_ok());

    image.seek(SeekFrom::Start(1024 + 0x38)).unwrap();
    image.write_all(&[0, 0]).unwrap();
    image.flush().unwrap();
    assert!(validate_ext4_image(&image_path).is_err());
}

#[test]
fn resource_slot_claims_are_atomic_and_owner_checked() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("slots")).unwrap();
    let first = allocate_resource_slot(directory.path(), "fc-first").unwrap();
    let second = allocate_resource_slot(directory.path(), "fc-second").unwrap();
    assert_ne!(first, second);
    assert!(release_resource_slot(directory.path(), first, "fc-second").is_err());
    release_resource_slot(directory.path(), first, "fc-first").unwrap();
    assert!(!resource_slot_path(directory.path(), first).exists());
}

#[test]
fn machine_capacity_allows_reuse_but_rejects_an_additional_vm() {
    let max_machines = NonZeroUsize::new(2).unwrap();
    ensure_machine_capacity(max_machines, 1, false).unwrap();
    ensure_machine_capacity(max_machines, 2, true).unwrap();
    assert!(ensure_machine_capacity(max_machines, 2, false).is_err());
}

#[test]
fn machine_capacity_counts_launch_reservations() {
    let max_machines = NonZeroUsize::new(1).unwrap();
    let mut starting = HashSet::new();

    admit_machine_capacity(max_machines, &[], &mut starting, "fc-first").unwrap();
    assert!(admit_machine_capacity(max_machines, &[], &mut starting, "fc-second").is_err());
    assert!(starting.remove("fc-first"));
    admit_machine_capacity(max_machines, &[], &mut starting, "fc-second").unwrap();
}

#[test]
fn machine_capacity_does_not_double_count_a_live_relaunch() {
    let max_machines = NonZeroUsize::new(1).unwrap();
    let live = vec!["fc-first".to_string()];
    let mut starting = HashSet::new();

    admit_machine_capacity(max_machines, &live, &mut starting, "fc-first").unwrap();
    assert_eq!(starting, HashSet::from(["fc-first".to_string()]));
}

#[test]
fn state_root_lock_is_exclusive() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("backend.lock");
    let first = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    let second = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    flock(&first, FlockOperation::NonBlockingLockExclusive).unwrap();
    assert!(flock(&second, FlockOperation::NonBlockingLockExclusive).is_err());
}

#[test]
fn capacity_scan_fails_closed_on_invalid_manifest() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    fs::write(manifests.join("fc-invalid.json"), b"not json").unwrap();
    assert!(machine_capacity_state(directory.path(), |_| false).is_err());
}

#[test]
fn capacity_scan_counts_processes_instead_of_manifests() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("manifests")).unwrap();
    let live = MachineRecord {
        machine_id: "fc-live".to_string(),
        spec_hash: "live".to_string(),
        runtime: test_runtime(),
        resolved_image: "/images/base.ext4".to_string(),
        slot: 1,
        network_enabled: false,
        workspace_id: None,
        idle_ttl_seconds: Some(60),
        snapshot_template: None,
        snapshot_network_slot: None,
    };
    let dead = MachineRecord {
        machine_id: "fc-dead".to_string(),
        spec_hash: "dead".to_string(),
        slot: 2,
        ..live.clone()
    };
    write_manifest(directory.path(), &live).unwrap();
    write_manifest(directory.path(), &dead).unwrap();

    let capacity =
        machine_capacity_state(directory.path(), |machine_id| machine_id == live.machine_id)
            .unwrap();
    assert_eq!(capacity.live_machine_ids, vec![live.machine_id]);
    assert_eq!(capacity.dead_machine_ids, vec![dead.machine_id]);
}

#[test]
fn snapshot_resource_slots_form_a_reusable_dense_pool() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("slots")).unwrap();
    let first = allocate_resource_slot_from(directory.path(), "fc-first", 0).unwrap();
    let second = allocate_resource_slot_from(directory.path(), "fc-second", 0).unwrap();
    assert_eq!(first, 0);
    assert_eq!(second, 1);
    release_resource_slot(directory.path(), first, "fc-first").unwrap();
    let reused = allocate_resource_slot_from(directory.path(), "fc-third", 0).unwrap();
    assert_eq!(reused, 0);
}

#[test]
fn manifest_publish_does_not_replace_an_existing_machine() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("manifests")).unwrap();
    let first = MachineRecord {
        machine_id: "fc-machine".to_string(),
        spec_hash: "first".to_string(),
        runtime: test_runtime(),
        resolved_image: "/images/base.ext4".to_string(),
        slot: 1,
        network_enabled: false,
        workspace_id: None,
        idle_ttl_seconds: Some(60),
        snapshot_template: None,
        snapshot_network_slot: None,
    };
    let second = MachineRecord {
        spec_hash: "second".to_string(),
        ..first.clone()
    };
    write_manifest(directory.path(), &first).unwrap();
    assert!(write_manifest(directory.path(), &second).is_err());
    let stored = serde_json::from_slice::<MachineRecord>(
        &fs::read(manifest_path(directory.path(), &first.machine_id)).unwrap(),
    )
    .unwrap();
    assert_eq!(stored.spec_hash, "first");
}

#[test]
fn persisted_lease_expires_machine_after_idle_ttl() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("leases")).unwrap();
    fs::create_dir(directory.path().join("manifests")).unwrap();
    let record = MachineRecord {
        machine_id: "fc-machine".to_string(),
        spec_hash: "spec".to_string(),
        runtime: test_runtime(),
        resolved_image: "/images/base.ext4".to_string(),
        slot: 1,
        network_enabled: false,
        workspace_id: None,
        idle_ttl_seconds: Some(60),
        snapshot_template: None,
        snapshot_network_slot: None,
    };
    write_manifest(directory.path(), &record).unwrap();
    touch_machine_lease(directory.path(), &record.machine_id).unwrap();
    let last_used = fs::metadata(lease_path(directory.path(), &record.machine_id))
        .unwrap()
        .modified()
        .unwrap();

    assert!(
        expired_machine_ids(directory.path(), last_used + Duration::from_secs(59))
            .unwrap()
            .is_empty()
    );
    assert!(
        !machine_lease_expired(
            directory.path(),
            &record.machine_id,
            last_used + Duration::from_secs(59)
        )
        .unwrap()
    );
    assert!(
        machine_lease_expired(
            directory.path(),
            &record.machine_id,
            last_used + Duration::from_secs(60)
        )
        .unwrap()
    );
    assert_eq!(
        expired_machine_ids(directory.path(), last_used + Duration::from_secs(60)).unwrap(),
        vec![record.machine_id]
    );
}

fn create_snapshot_template(
    state_root: &Path,
    key: &str,
    lifecycle: SnapshotTemplateLifecycle,
) -> PathBuf {
    let directory = state_root.join("snapshots").join(key);
    fs::create_dir(&directory).unwrap();
    File::create(directory.join("complete")).unwrap();
    File::create(directory.join(SNAPSHOT_LEASE_FILE)).unwrap();
    if lifecycle == SnapshotTemplateLifecycle::Machine {
        File::create(directory.join(SNAPSHOT_FORK_TEMPLATE_FILE)).unwrap();
    }
    directory
}

#[test]
fn snapshot_gc_reaps_only_unreferenced_fork_templates() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("snapshots")).unwrap();
    fs::create_dir(directory.path().join("manifests")).unwrap();
    let config = FirecrackerConfig {
        state_root: directory.path().to_path_buf(),
        ..FirecrackerConfig::default()
    };
    let orphaned_fork_key = "a".repeat(64);
    let explicit_key = "b".repeat(64);
    let referenced_key = "c".repeat(64);
    let orphaned_fork = create_snapshot_template(
        directory.path(),
        &orphaned_fork_key,
        SnapshotTemplateLifecycle::Machine,
    );
    let explicit = create_snapshot_template(
        directory.path(),
        &explicit_key,
        SnapshotTemplateLifecycle::Snapshot,
    );
    let referenced = create_snapshot_template(
        directory.path(),
        &referenced_key,
        SnapshotTemplateLifecycle::Machine,
    );
    write_manifest(
        directory.path(),
        &MachineRecord {
            machine_id: "fc-0123456789abcdef-01234567".to_string(),
            spec_hash: "spec".to_string(),
            runtime: test_runtime(),
            resolved_image: "/images/base.ext4".to_string(),
            slot: 1,
            network_enabled: false,
            workspace_id: None,
            idle_ttl_seconds: Some(60),
            snapshot_template: Some(SnapshotTemplateReference {
                key: referenced_key.clone(),
                lifecycle: SnapshotTemplateLifecycle::Machine,
            }),
            snapshot_network_slot: Some(0),
        },
    )
    .unwrap();

    assert_eq!(
        reap_orphaned_fork_snapshot_templates_blocking(&config).unwrap(),
        vec![orphaned_fork_key]
    );
    assert!(!orphaned_fork.exists());
    assert!(explicit.exists());
    assert!(referenced.exists());
    File::create(referenced.join(SNAPSHOT_DELETE_FILE)).unwrap();
    assert!(
        reap_orphaned_fork_snapshot_templates_blocking(&config)
            .unwrap()
            .is_empty()
    );
    fs::remove_file(manifest_path(
        directory.path(),
        "fc-0123456789abcdef-01234567",
    ))
    .unwrap();
    assert_eq!(
        reap_orphaned_fork_snapshot_templates_blocking(&config).unwrap(),
        vec![referenced_key]
    );
    assert!(!referenced.exists());
}

#[test]
fn snapshot_gc_does_not_delete_fork_between_capture_and_manifest_publish() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("snapshots")).unwrap();
    fs::create_dir(directory.path().join("manifests")).unwrap();
    let config = FirecrackerConfig {
        state_root: directory.path().to_path_buf(),
        ..FirecrackerConfig::default()
    };
    let key = "d".repeat(64);
    let snapshot =
        create_snapshot_template(directory.path(), &key, SnapshotTemplateLifecycle::Machine);
    let lease = open_snapshot_template_lease(&config, &key).unwrap();

    assert!(
        reap_orphaned_fork_snapshot_templates_blocking(&config)
            .unwrap()
            .is_empty()
    );
    assert!(snapshot.exists());
    write_manifest(
        directory.path(),
        &MachineRecord {
            machine_id: "fc-0123456789abcdef-01234567".to_string(),
            spec_hash: "spec".to_string(),
            runtime: test_runtime(),
            resolved_image: "/images/base.ext4".to_string(),
            slot: 1,
            network_enabled: false,
            workspace_id: None,
            idle_ttl_seconds: Some(60),
            snapshot_template: Some(SnapshotTemplateReference {
                key,
                lifecycle: SnapshotTemplateLifecycle::Machine,
            }),
            snapshot_network_slot: Some(0),
        },
    )
    .unwrap();
    drop(lease);
    assert!(
        reap_orphaned_fork_snapshot_templates_blocking(&config)
            .unwrap()
            .is_empty()
    );
    assert!(snapshot.exists());
}

#[test]
fn snapshot_gc_preserves_templates_when_manifest_scan_fails() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("snapshots")).unwrap();
    fs::create_dir(directory.path().join("manifests")).unwrap();
    let config = FirecrackerConfig {
        state_root: directory.path().to_path_buf(),
        ..FirecrackerConfig::default()
    };
    let key = "e".repeat(64);
    let snapshot =
        create_snapshot_template(directory.path(), &key, SnapshotTemplateLifecycle::Machine);
    fs::write(directory.path().join("manifests/corrupt.json"), b"{").unwrap();

    assert!(reap_orphaned_fork_snapshot_templates_blocking(&config).is_err());
    assert!(snapshot.exists());
}

#[test]
fn fork_snapshots_are_unique_per_target() {
    let source = MachineRecord {
        machine_id: "fc-source".to_string(),
        spec_hash: "spec".to_string(),
        runtime: test_runtime(),
        resolved_image: "/images/base.ext4".to_string(),
        slot: 1,
        network_enabled: false,
        workspace_id: None,
        idle_ttl_seconds: Some(60),
        snapshot_template: Some(SnapshotTemplateReference {
            key: "a".repeat(64),
            lifecycle: SnapshotTemplateLifecycle::Machine,
        }),
        snapshot_network_slot: None,
    };

    let first = fork_snapshot_template_key(&source, "fc-target-1");
    assert_eq!(first, fork_snapshot_template_key(&source, "fc-target-1"));
    assert_ne!(first, fork_snapshot_template_key(&source, "fc-target-2"));

    let mut second_source = source;
    second_source.machine_id = "fc-source-2".to_string();
    assert_ne!(
        first,
        fork_snapshot_template_key(&second_source, "fc-target-1")
    );
}

#[test]
fn explicit_snapshots_are_unique_and_reusable() {
    let source = MachineRecord {
        machine_id: "fc-source".to_string(),
        spec_hash: "spec".to_string(),
        runtime: test_runtime(),
        resolved_image: "/images/base.ext4".to_string(),
        slot: 7,
        network_enabled: true,
        workspace_id: None,
        idle_ttl_seconds: Some(60),
        snapshot_template: None,
        snapshot_network_slot: None,
    };
    let first_id = Uuid::from_u128(1);
    let second_id = Uuid::from_u128(2);
    let first = explicit_snapshot_template_key(&source, first_id);
    assert_eq!(first, explicit_snapshot_template_key(&source, first_id));
    assert_ne!(first, explicit_snapshot_template_key(&source, second_id));

    let manifest = FirecrackerSnapshotManifest {
        format_version: SNAPSHOT_FORMAT_VERSION,
        template_key: first,
        spec_hash: source.spec_hash,
        source_network_slot: source.slot,
        runtime: source.runtime,
    };
    let encoded = serde_json::to_vec(&manifest).unwrap();
    let decoded: FirecrackerSnapshotManifest = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded.format_version, SNAPSHOT_FORMAT_VERSION);
    assert_eq!(decoded.source_network_slot, 7);
    validate_snapshot_key(&decoded.template_key).unwrap();
}

#[tokio::test]
async fn vsock_request_uses_firecracker_handshake_and_framing() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("exo.vsock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = AsyncBufReader::new(stream);
        let mut handshake = String::new();
        stream.read_line(&mut handshake).await.unwrap();
        assert_eq!(handshake, "CONNECT 10052\n");
        stream
            .get_mut()
            .write_all(b"OK 1073741824\n")
            .await
            .unwrap();
        stream.get_mut().flush().await.unwrap();
        let mut stream = stream.into_inner();
        let length = stream.read_u32().await.unwrap() as usize;
        let mut request = vec![0; length];
        stream.read_exact(&mut request).await.unwrap();
        assert_eq!(request, br#"{"type":"ping"}"#);
        let response = br#"{"ok":true}"#;
        stream
            .write_all(&(response.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stream.write_all(response).await.unwrap();
    });

    let response = vsock_request(&socket, br#"{"type":"ping"}"#).await.unwrap();
    assert_eq!(response, br#"{"ok":true}"#);
    server.await.unwrap();
}

#[test]
fn deleted_snapshot_waits_for_restore_lease_and_refuses_new_restores() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("snapshots")).unwrap();
    fs::create_dir(directory.path().join("manifests")).unwrap();
    let config = FirecrackerConfig {
        state_root: directory.path().to_path_buf(),
        ..Default::default()
    };
    let key = "f".repeat(64);
    let snapshot =
        create_snapshot_template(directory.path(), &key, SnapshotTemplateLifecycle::Snapshot);
    let lease = open_snapshot_template_lease(&config, &key).unwrap();
    File::create(snapshot.join(SNAPSHOT_DELETE_FILE)).unwrap();
    assert!(open_snapshot_template_lease(&config, &key).is_err());
    assert!(
        reap_orphaned_fork_snapshot_templates_blocking(&config)
            .unwrap()
            .is_empty()
    );
    assert!(snapshot.exists());
    drop(lease);
    assert_eq!(
        reap_orphaned_fork_snapshot_templates_blocking(&config).unwrap(),
        vec![key]
    );
    assert!(!snapshot.exists());
}

#[test]
fn persistent_snapshot_expiry_reclaims_files() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("snapshots")).unwrap();
    fs::create_dir(directory.path().join("manifests")).unwrap();
    let config = FirecrackerConfig {
        state_root: directory.path().to_path_buf(),
        ..Default::default()
    };
    let key = "f".repeat(64);
    let snapshot =
        create_snapshot_template(directory.path(), &key, SnapshotTemplateLifecycle::Snapshot);
    let lease = File::open(snapshot.join(SNAPSHOT_LEASE_FILE)).unwrap();
    lease
        .set_times(std::fs::FileTimes::new().set_modified(SystemTime::now() - SNAPSHOT_MAX_AGE))
        .unwrap();
    assert!(open_snapshot_template_lease(&config, &key).is_err());
    assert_eq!(
        reap_orphaned_fork_snapshot_templates_blocking(&config).unwrap(),
        vec![key]
    );
    assert!(!snapshot.exists());
}

#[test]
fn snapshot_budget_counts_retained_logical_bytes_and_pending_capture() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("snapshots")).unwrap();
    let config = FirecrackerConfig {
        state_root: directory.path().to_path_buf(),
        ..Default::default()
    };
    let snapshot = create_snapshot_template(
        directory.path(),
        &"a".repeat(64),
        SnapshotTemplateLifecycle::Snapshot,
    );
    File::create(snapshot.join("memory"))
        .unwrap()
        .set_len(1024)
        .unwrap();
    assert!(enforce_snapshot_budget(&config, MAX_SNAPSHOT_BYTES - 1024).is_ok());
    assert!(enforce_snapshot_budget(&config, MAX_SNAPSHOT_BYTES - 1023).is_err());
    assert!(enforce_snapshot_budget(&config, u64::MAX).is_err());

    let machine = jail_dir(&config, "fc-0000000000000000-00000000");
    fs::create_dir_all(&machine).unwrap();
    fs::hard_link(snapshot.join("memory"), machine.join("snapshot-memory")).unwrap();
    assert!(enforce_snapshot_budget(&config, MAX_SNAPSHOT_BYTES - 2048).is_ok());
    assert!(enforce_snapshot_budget(&config, MAX_SNAPSHOT_BYTES - 2047).is_err());
    fs::remove_dir_all(snapshot).unwrap();
    assert!(enforce_snapshot_budget(&config, MAX_SNAPSHOT_BYTES - 1024).is_ok());
    assert!(enforce_snapshot_budget(&config, MAX_SNAPSHOT_BYTES - 1023).is_err());
}

fn test_shared(
    config: FirecrackerConfig,
    warm_machines: HashMap<SandboxId, WarmMachineEntry>,
) -> Result<Arc<Shared>> {
    let state_lock = File::create(config.state_root.join("backend.lock"))?;
    Ok(Arc::new(Shared {
        config,
        host_fingerprint: test_host_runtime(),
        _state_lock: state_lock,
        warm_machines: Mutex::new(warm_machines),
        egress_transports: StdMutex::new(HashMap::new()),
        lifecycle_locks: MachineLifecycleLocks::default(),
        capacity_gate: Mutex::new(()),
        starting_machines: Arc::new(StdMutex::new(HashSet::new())),
    }))
}

#[tokio::test]
async fn idle_reap_closes_egress_before_machine_cleanup_can_fail() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for name in ["manifests", "leases"] {
        fs::create_dir(directory.path().join(name))?;
    }
    let config = FirecrackerConfig {
        state_root: directory.path().into(),
        ..Default::default()
    };
    let record = MachineRecord {
        machine_id: "fc-egress-reaped".into(),
        spec_hash: "test".into(),
        runtime: test_runtime(),
        resolved_image: "/images/test.ext4".into(),
        slot: 1,
        network_enabled: false,
        workspace_id: None,
        idle_ttl_seconds: Some(0),
        snapshot_template: None,
        snapshot_network_slot: None,
    };
    write_manifest(directory.path(), &record)?;
    let shared = test_shared(config, HashMap::new())?;
    let pid_path = shared.pid_path(&record.machine_id);
    fs::create_dir_all(pid_path.parent().unwrap())?;
    fs::write(&pid_path, "invalid-pid")?;
    let listener = Arc::new(LocalEgressTransport::for_hosts(&["api.test".into()]).await?);
    shared
        .egress_transports
        .lock()
        .unwrap()
        .insert(record.machine_id.clone(), listener.clone());
    let backend = FirecrackerSandboxBackend {
        shared: shared.clone(),
        egress: Arc::new(EgressRuntime::new(None, Arc::new(PublicUpstreamResolver))),
    };
    assert_eq!(shared.is_running(&record.machine_id).await?, Some(false));
    assert!(backend.reap_expired_machines().await.is_err());
    assert!(listener.is_closed());
    assert!(shared.egress_transports.lock().unwrap().is_empty());
    let endpoints = listener.endpoints();
    let replacement = LocalEgressTransport::with_config(
        crate::EgressListenConfig {
            bind_address: *endpoints.http.ip(),
            advertised_address: *endpoints.http.ip(),
            http_port: endpoints.http.port(),
            https_port: endpoints.https.port(),
            dns_port: endpoints.dns.port(),
        },
        &[],
    )
    .await?;
    replacement.close();
    Ok(())
}

struct DurableStopFixture {
    _directory: tempfile::TempDir,
    backend: FirecrackerSandboxBackend,
    request: SandboxRequest,
    machine: Machine,
    listener: UnixListener,
    handle: Arc<FirecrackerSandboxHandle>,
    egress_transport: Arc<LocalEgressTransport>,
}

impl DurableStopFixture {
    async fn new() -> Result<Self> {
        let directory = tempfile::tempdir_in("/tmp")?;
        for name in ["manifests", "leases", "slots"] {
            fs::create_dir(directory.path().join(name))?;
        }
        let config = FirecrackerConfig {
            state_root: directory.path().into(),
            ..Default::default()
        };
        let request = SandboxRequest {
            sandbox_id: "durable".into(),
            scope: None,
            spec: SandboxSpec {
                image: "/images/test.ext4".into(),
                resources: Default::default(),
                mounts: vec![],
                durable_file_systems: vec![],
                policy: SandboxNetworkPolicy::Limited {
                    allowed_hosts: vec!["api.test".into()],
                }
                .into(),
                default_workdir: "/workspace".into(),
            },
            lifecycle: crate::SandboxLifecycleConfig {
                idle_ttl: Some(Duration::from_secs(60)),
            },
            provider_state: None,
        };
        let record = MachineRecord {
            machine_id: "fc-durable".into(),
            spec_hash: sandbox_spec_hash(&request.spec),
            runtime: test_runtime(),
            resolved_image: request.spec.image.clone(),
            slot: 1,
            network_enabled: false,
            workspace_id: Some("workspace".into()),
            idle_ttl_seconds: None,
            snapshot_template: None,
            snapshot_network_slot: None,
        };
        write_manifest(directory.path(), &record)?;
        let shared = test_shared(
            config,
            HashMap::from([(
                request.sandbox_id.clone(),
                WarmMachineEntry {
                    egress_proxy: None,
                    machine_id: record.machine_id.clone(),
                    spec_hash: record.spec_hash.clone(),
                    idle_ttl: None,
                    last_used_at: Instant::now(),
                },
            )]),
        )?;
        let machine = machine_from_record(&shared.config, record);
        fs::create_dir_all(machine.vsock_path.parent().unwrap())?;
        let listener = UnixListener::bind(&machine.vsock_path)?;
        // Only liveness reads this PID. The fake guest removes it before a
        // reply that permits cleanup so it never signals the test process.
        fs::write(
            shared.pid_path(&machine.record.machine_id),
            std::process::id().to_string(),
        )?;
        let backend = FirecrackerSandboxBackend {
            shared,
            egress: Arc::new(EgressRuntime::new(None, Arc::new(PublicUpstreamResolver))),
        };
        let egress_transport =
            Arc::new(LocalEgressTransport::for_hosts(&["api.test".into()]).await?);
        backend
            .shared
            .egress_transports
            .lock()
            .unwrap()
            .insert(machine.record.machine_id.clone(), egress_transport.clone());
        let handle = backend
            .egress
            .acquire(
                request.clone(),
                |_| async { Ok(egress_transport.clone() as Arc<dyn EgressTransport>) },
                |egress| async {
                    Ok(FirecrackerSandboxHandle {
                        egress,
                        id: request.sandbox_id.clone(),
                        machine: machine.clone(),
                        request: request.clone().into(),
                        spec_hash: machine.record.spec_hash.clone(),
                        shared: backend.shared.clone(),
                        one_shot: false,
                    })
                },
                async { panic!("successful acquisition must not terminate the sandbox") },
            )
            .await?;
        Ok(Self {
            _directory: directory,
            backend,
            request,
            machine,
            listener,
            handle,
            egress_transport,
        })
    }

    async fn sync_request(&self) -> Result<UnixStream> {
        let (stream, _) = self.listener.accept().await?;
        let mut stream = AsyncBufReader::new(stream);
        let mut handshake = String::new();
        stream.read_line(&mut handshake).await?;
        assert_eq!(handshake, "CONNECT 10052\n");
        stream.get_mut().write_all(b"OK 1073741824\n").await?;
        let mut stream = stream.into_inner();
        let length = stream.read_u32().await? as usize;
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).await?;
        let request: Message<ProtocolGuestRequest<()>> = serde_json::from_slice(&payload)?;
        assert!(
            matches!(request.payload, ProtocolGuestRequest::SyncFilesystem { path } if path == "/workspace")
        );
        Ok(stream)
    }

    async fn sync_reply(&self, response: GuestResponse, exit_guest: bool) -> Result<()> {
        let mut stream = self.sync_request().await?;
        if exit_guest {
            fs::remove_file(
                self.backend
                    .shared
                    .pid_path(&self.machine.record.machine_id),
            )?;
        }
        let payload = serde_json::to_vec(&Message::new(response))?;
        stream.write_u32(payload.len() as u32).await?;
        stream.write_all(&payload).await?;
        Ok(())
    }
}

#[tokio::test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "uses Linux process liveness; no VM required"
)]
async fn terminate_flushes_durable_guest_without_acquiring_a_vm() -> Result<()> {
    let fixture = DurableStopFixture::new().await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::try_join!(
            fixture.backend.terminate(fixture.request.clone()),
            fixture.sync_reply(GuestResponse::ok(), true),
        )
    })
    .await??;
    assert!(
        !fixture
            .backend
            .shared
            .manifest_path(&fixture.machine.record.machine_id)
            .exists()
    );
    assert!(fixture.backend.shared.warm_machines.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "uses Linux process liveness; no VM required"
)]
async fn failed_stop_keeps_the_vm_and_egress_usable_until_retry() -> Result<()> {
    let fixture = DurableStopFixture::new().await?;
    let (stop, reply) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            fixture.handle.stop(),
            fixture.sync_reply(GuestResponse::error("sync failed"), false),
        )
    })
    .await?;
    reply?;
    assert!(format!("{:#}", stop.unwrap_err()).contains("sync failed"));
    assert!(!fixture.egress_transport.is_closed());
    assert!(
        fixture
            .backend
            .shared
            .pid_path(&fixture.machine.record.machine_id)
            .exists()
    );
    assert!(
        fixture
            .backend
            .shared
            .manifest_path(&fixture.machine.record.machine_id)
            .exists()
    );
    assert!(
        fixture
            .backend
            .shared
            .warm_machines
            .lock()
            .await
            .contains_key(&fixture.request.sandbox_id)
    );
    // The non-booting request path used by Lima must preserve the same state.
    let (stop, reply) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            fixture.backend.stop_request(fixture.request.clone()),
            fixture.sync_reply(GuestResponse::error("sync failed"), false),
        )
    })
    .await?;
    reply?;
    assert!(format!("{:#}", stop.unwrap_err()).contains("sync failed"));
    assert!(!fixture.egress_transport.is_closed());
    // A subsequent successful stop closes egress even while the handle is retained.
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::try_join!(
            fixture.handle.stop(),
            fixture.sync_reply(GuestResponse::ok(), true)
        )
    })
    .await??;
    assert!(fixture.egress_transport.is_closed());
    assert!(
        !fixture
            .backend
            .shared
            .manifest_path(&fixture.machine.record.machine_id)
            .exists()
    );
    Ok(())
}

#[tokio::test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "uses Linux process liveness; no VM required"
)]
async fn terminate_cleans_up_after_a_failed_durable_sync() -> Result<()> {
    let fixture = DurableStopFixture::new().await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::try_join!(
            fixture.backend.terminate(fixture.request.clone()),
            fixture.sync_reply(GuestResponse::error("sync failed"), true),
        )
    })
    .await??;
    assert!(fixture.egress_transport.is_closed());
    assert!(
        !fixture
            .backend
            .shared
            .manifest_path(&fixture.machine.record.machine_id)
            .exists()
    );
    assert!(fixture.backend.shared.warm_machines.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "uses Linux process liveness; no VM required"
)]
async fn terminate_cleans_up_when_the_guest_never_answers_sync() -> Result<()> {
    let fixture = DurableStopFixture::new().await?;
    let (finished, completion) = tokio::sync::oneshot::channel();
    let terminate = async {
        let result = fixture.backend.terminate(fixture.request.clone()).await;
        finished.send(()).unwrap();
        result
    };
    let silent_guest = async {
        let stream = fixture.sync_request().await?;
        // Simulate VM exit so cleanup can run without signaling the test process,
        // but hold its connection open to exercise the sync timeout, not EOF.
        fs::remove_file(
            fixture
                .backend
                .shared
                .pid_path(&fixture.machine.record.machine_id),
        )?;
        tokio::time::pause();
        tokio::time::advance(GUEST_REQUEST_TIMEOUT + Duration::from_secs(1)).await;
        tokio::time::resume();
        completion.await?;
        drop(stream);
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(GUEST_REQUEST_TIMEOUT * 2, async {
        tokio::try_join!(terminate, silent_guest)
    })
    .await??;
    assert!(fixture.egress_transport.is_closed());
    assert!(
        !fixture
            .backend
            .shared
            .manifest_path(&fixture.machine.record.machine_id)
            .exists()
    );
    assert!(fixture.backend.shared.warm_machines.lock().await.is_empty());
    Ok(())
}
