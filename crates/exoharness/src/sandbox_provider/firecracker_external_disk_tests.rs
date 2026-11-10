use super::super::tests::{native_request, shell};
use super::*;
use crate::SnapshotKind;
use std::os::unix::fs::FileTypeExt;
use std::sync::atomic::{AtomicBool, AtomicUsize};

// A real block device supplies bytes missing from the sparse backing file.
// This exercises jail attachment and capture ownership without a ublk daemon.
struct LoopDisk {
    backing: PathBuf,
    full: PathBuf,
    device: PathBuf,
    complete: AtomicBool,
    fail: AtomicBool,
    captures: AtomicUsize,
}

impl LoopDisk {
    fn new(directory: &Path, index: usize, source: &Path) -> Result<Arc<Self>> {
        let full = directory.join(format!("loop-{index}.ext4"));
        copy_sparse_reflink(source, &full)?;
        fs::set_permissions(&full, Permissions::from_mode(0o600))?;
        let backing = directory.join(format!("partial-{index}.ext4"));
        File::create(&backing)?.set_len(fs::metadata(source)?.len())?;
        let output = Command::new("losetup")
            .args(["--find", "--show"])
            .arg(&full)
            .output()?;
        ensure!(
            output.status.success(),
            "losetup: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(Arc::new(Self {
            device: String::from_utf8(output.stdout)?.trim().into(),
            backing,
            full,
            complete: AtomicBool::new(false),
            fail: AtomicBool::new(false),
            captures: AtomicUsize::new(0),
        }))
    }
}

impl FirecrackerFilesystemDisk for LoopDisk {
    fn backing_file(&self) -> &Path {
        &self.backing
    }
    fn device_path(&self) -> &Path {
        &self.device
    }
    fn capture(&self, destination: &Path) -> Result<bool> {
        self.captures.fetch_add(1, Ordering::SeqCst);
        ensure!(
            !self.fail.swap(false, Ordering::SeqCst),
            "injected disk capture failure"
        );
        let complete = self.complete.load(Ordering::SeqCst);
        copy_sparse_reflink(
            if complete { &self.full } else { &self.backing },
            destination,
        )?;
        Ok(complete)
    }
}

impl Drop for LoopDisk {
    fn drop(&mut self) {
        match Command::new("losetup")
            .arg("--detach")
            .arg(&self.device)
            .status()
        {
            Ok(status) if status.success() => {}
            result => eprintln!("loop device cleanup: {result:?}"),
        }
        if let Err(error) = fs::remove_file(&self.backing) {
            eprintln!("backing cleanup: {error}");
        }
    }
}

fn config(root: &Path, image: &str) -> FirecrackerConfig {
    FirecrackerConfig {
        state_root: root.join("s"),
        allowed_local_images: vec![image.into()],
        image_size_gib: 1,
        workspace_size_gib: 1,
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "requires Linux root, KVM, loop devices, Python guest image, and XFS TMPDIR"]
async fn native_external_disks_capture_and_recover() -> Result<()> {
    let temp = tempfile::Builder::new().prefix("ext-").tempdir()?;
    let image = std::env::var("FIRECRACKER_IMAGE")?;
    let config = config(temp.path(), &image);
    let backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    let mut request = native_request(&config, "external", &image)?;
    let result: Result<()> = async {
        let source = backend.acquire(request.clone()).await?;
        shell(&source, "sudo sh -c 'printf root > /proof'; printf workspace > /workspace/proof; printf repository > /repo/proof").await?;
        request.provider_state = source.provider_state();
        let snapshot = backend.suspend(request.clone(), SnapshotKind::Filesystem).await?;
        drop(source);
        request.provider_state = None;
        let files = backend.filesystem_snapshot_files(snapshot.clone()).await?;
        // An unverified lazy base must never poison the shared digest cache.
        let mut wrong = FilesystemManifest::from_payload(&snapshot)?;
        wrong.base_image_sha256 = "0".repeat(64);
        for lazy in [true, false] {
            let result = if lazy {
                backend.import_lazy_filesystem_snapshot(wrong.payload()?, files.base_image.clone(), files.disks.clone()).await
            } else {
                backend.import_filesystem_snapshot(wrong.payload()?, files.base_image.clone(), files.disks.clone()).await
            };
            assert!(result.unwrap_err().to_string().contains("base image does not match"));
            assert!(!base_image_cache_path(&config, &wrong.base_image_sha256).exists());
        }
        let disks = files.disks.iter().enumerate()
            .map(|(i, source)| LoopDisk::new(temp.path(), i, source)).collect::<Result<Vec<_>>>()?;
        let partial = backend.import_lazy_filesystem_snapshot(snapshot, files.base_image.clone(),
            disks.iter().map(|disk| disk.backing.clone()).collect()).await?;
        let registration = backend.register_filesystem_disks(request.sandbox_id.clone(), partial.clone(),
            disks.iter().cloned().map(|disk| disk as Arc<dyn FirecrackerFilesystemDisk>).collect())?;
        let handle = backend.acquire_from_snapshot(request.clone(), partial).await?;
        let record = backend.shared.warm_machines.lock().await[&request.sandbox_id].machine_id.clone();
        let names = FilesystemLayout::from_spec(&request.spec).disk_names();
        for (name, disk) in names.iter().zip(&disks) {
            let jailed = jail_root(&config, &record).join(name);
            assert_eq!((fs::metadata(&jailed)?.dev(), fs::metadata(&jailed)?.ino()),
                (fs::metadata(&disk.backing)?.dev(), fs::metadata(&disk.backing)?.ino()));
            let node = fs::metadata(jailed.with_extension("ublk"))?;
            assert!(node.file_type().is_block_device());
            assert_eq!(node.rdev(), fs::metadata(&disk.device)?.rdev());
            assert_eq!(fs::metadata(&disk.backing)?.blocks(), 0);
        }
        assert_eq!(shell(&handle, "cat /proof /workspace/proof /repo/proof").await?, "rootworkspacerepository");
        let record = backend.shared.load_machine_record(&record).await?.context("external record")?;
        let mut resumed = request.clone();
        resumed.spec.image = record.resolved_image.clone();
        resumed.provider_state = handle.provider_state();
        backend.shared.stop_machine_process(&record.machine_id).await?;
        let handle: Arc<dyn ManagedSandboxHandle> = Arc::new(backend.acquire_resolved(
            FirecrackerRequest { sandbox: resumed, egress_proxy: None }).await?);
        assert_eq!(shell(&handle, "cat /proof /workspace/proof /repo/proof").await?, "rootworkspacerepository");

        shell(&handle, "sudo sh -c 'printf grown > /proof'; printf updated > /workspace/proof; printf latest > /repo/proof").await?;
        // A resource/spec change must not silently switch to sparse local files.
        let mut changed = request.clone();
        changed.provider_state = handle.provider_state();
        changed.spec.resources = SandboxResourceShape::new(1, 1536);
        assert!(backend.acquire(changed).await.err().context("replacement accepted")?
            .to_string().contains("external disks require filesystem suspend"));
        assert_eq!(shell(&handle, "printf alive").await?, "alive");
        assert!(handle.snapshot(SnapshotKind::Full).await.is_err());
        disks[0].fail.store(true, Ordering::SeqCst);
        assert!(handle.snapshot(SnapshotKind::Filesystem).await.is_err());
        assert_eq!(shell(&handle, "cat /proof").await?, "grown");
        drop(registration);
        let weak: Vec<_> = disks.iter().map(Arc::downgrade).collect();
        drop(disks);
        assert!(weak.iter().all(|disk| disk.upgrade().is_some()));
        let partial = handle.snapshot(SnapshotKind::Filesystem).await?;
        assert_eq!(partial.format, SnapshotFormat::FirecrackerLazyFilesystemRef);
        assert!(!backend.filesystem_snapshot_files(partial.clone()).await?.complete);
        assert!(backend.acquire_from_snapshot(request.clone(), partial).await.err()
            .context("partial restore accepted without registration")?.to_string().contains("requires external disks"));
        for disk in &weak { disk.upgrade().context("disk owner lost")?.complete.store(true, Ordering::SeqCst); }
        let full = handle.snapshot(SnapshotKind::Filesystem).await?;
        assert!(backend.filesystem_snapshot_files(full.clone()).await?.complete);
        handle.stop().await?;
        drop(handle);
        assert!(weak.iter().all(|disk| disk.upgrade().is_none()));
        let restored = backend.acquire_from_snapshot(request.clone(), full).await?;
        assert_eq!(shell(&restored, "cat /proof /workspace/proof /repo/proof").await?, "grownupdatedlatest");
        Ok(())
    }.await;
    let cleanup = backend.terminate_all().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires Linux root, KVM, loop devices, Python guest image, and XFS TMPDIR"]
async fn native_external_restart_and_stale_registration() -> Result<()> {
    let temp = tempfile::Builder::new().prefix("ext-").tempdir()?;
    let image = std::env::var("FIRECRACKER_IMAGE")?;
    let config = config(temp.path(), &image);
    let backend = FirecrackerSandboxBackend::new(config.clone()).await?;
    let mut request = native_request(&config, "restart", &image)?;
    request.spec.mounts.clear();
    request.spec.durable_file_systems.clear();
    request.spec.default_workdir = "/".into();
    let result: Result<SnapshotPayload> = async {
        let source = backend.acquire(request.clone()).await?;
        let state = parse_provider_state(&source.provider_state().context("provider state")?)?;
        let local_image = backend
            .shared
            .load_machine_record(&state.machine_id)
            .await?
            .context("source record")?
            .resolved_image;
        shell(&source, "sudo sh -c 'printf external > /proof'").await?;
        let snapshot = backend
            .suspend(request.clone(), SnapshotKind::Filesystem)
            .await?;
        drop(source);
        let files = backend.filesystem_snapshot_files(snapshot.clone()).await?;
        let disk = LoopDisk::new(temp.path(), 0, &files.disks[0])?;
        let partial = backend
            .import_lazy_filesystem_snapshot(
                snapshot,
                files.base_image.clone(),
                vec![disk.backing.clone()],
            )
            .await?;
        let registration = backend.register_filesystem_disks(
            request.sandbox_id.clone(),
            partial.clone(),
            vec![disk.clone()],
        )?;
        let handle = backend
            .acquire_from_snapshot(request.clone(), partial)
            .await?;
        let id = backend.shared.warm_machines.lock().await[&request.sandbox_id]
            .machine_id
            .clone();
        let record = backend
            .shared
            .load_machine_record(&id)
            .await?
            .context("external record")?;
        let mut resolved = request.clone();
        resolved.spec.image = record.resolved_image.clone();
        resolved.provider_state = handle.provider_state();
        let resolved = FirecrackerRequest {
            sandbox: resolved,
            egress_proxy: None,
        };
        backend.shared.stop_machine_process(&id).await?;
        let ownership = backend
            .shared
            .active_filesystem_disks
            .lock()
            .expect("disk owners")
            .remove(&id)
            .context("active ownership")?;
        assert!(
            backend
                .acquire_resolved(resolved.clone())
                .await
                .err()
                .context("ownerless restart accepted")?
                .to_string()
                .contains("ownership is unavailable")
        );
        assert!(!process_running(&backend.shared.pid_path(&id)));
        backend
            .shared
            .active_filesystem_disks
            .lock()
            .expect("disk owners")
            .insert(id.clone(), ownership);
        let restarted: Arc<dyn ManagedSandboxHandle> =
            Arc::new(backend.acquire_resolved(resolved.clone()).await?);
        assert_eq!(shell(&restarted, "cat /proof").await?, "external");
        backend.shared.stop_machine_process(&id).await?;
        // A node pointing to another live block device cannot be reused on restart.
        let other = LoopDisk::new(temp.path(), 1, &files.disks[0])?;
        let node = jail_root(&config, &id).join("overlay.ublk");
        fs::remove_file(&node)?;
        install_block_device(&other.device, &node, jailer_uid(&config, &record)?)?;
        assert!(
            backend
                .acquire_resolved(resolved.clone())
                .await
                .err()
                .context("stale node accepted")?
                .to_string()
                .contains("generation changed")
        );
        assert!(!process_running(&backend.shared.pid_path(&id)));
        fs::remove_file(&node)?;
        install_block_device(&disk.device, &node, jailer_uid(&config, &record)?)?;
        let restarted: Arc<dyn ManagedSandboxHandle> =
            Arc::new(backend.acquire_resolved(resolved).await?);
        assert_eq!(shell(&restarted, "cat /proof").await?, "external");
        restarted.stop().await?;
        drop(restarted);
        drop(handle);
        let calls = disk.captures.load(Ordering::SeqCst);
        // The registration remains alive but belongs to a terminated machine.
        let mut local = request.clone();
        local.spec.image = local_image;
        local.spec.resources = SandboxResourceShape::new(1, 1536);
        let local_id = machine_id(&local.sandbox_id, &sandbox_spec_hash(&local.spec));
        let stale = jail_root(&config, &local_id).join("overlay.ublk");
        fs::create_dir_all(stale.parent().context("stale parent")?)?;
        install_block_device(&other.device, &stale, 0)?;
        let local: Arc<dyn ManagedSandboxHandle> = Arc::new(
            backend
                .acquire_resolved(FirecrackerRequest {
                    sandbox: local,
                    egress_proxy: None,
                })
                .await?,
        );
        assert_eq!(
            shell(&local, "test ! -e /proof; printf local").await?,
            "local"
        );
        let captured = local.snapshot(SnapshotKind::Filesystem).await?;
        assert_eq!(captured.format, SnapshotFormat::FirecrackerFilesystemRef);
        assert_eq!(disk.captures.load(Ordering::SeqCst), calls);
        drop(registration);
        Ok(captured)
    }
    .await;
    let cleanup = backend.terminate_all().await;
    let snapshot = result?;
    cleanup?;

    // The last controller owner must stop its VMM before releasing device owners.
    let files = backend.filesystem_snapshot_files(snapshot.clone()).await?;
    let disk = LoopDisk::new(temp.path(), 2, &files.disks[0])?;
    let partial = backend
        .import_lazy_filesystem_snapshot(
            snapshot,
            files.base_image.clone(),
            vec![disk.backing.clone()],
        )
        .await?;
    let registration = backend.register_filesystem_disks(
        request.sandbox_id.clone(),
        partial.clone(),
        vec![disk.clone()],
    )?;
    let handle = backend
        .acquire_from_snapshot(request.clone(), partial)
        .await?;
    let id = backend.shared.warm_machines.lock().await[&request.sandbox_id]
        .machine_id
        .clone();
    let record = backend
        .shared
        .load_machine_record(&id)
        .await?
        .context("external record")?;
    let pid = backend.shared.pid_path(&id);
    let weak = Arc::downgrade(&disk);
    request.spec.image = record.resolved_image.clone();
    request.provider_state = handle.provider_state();
    drop(disk);
    drop(registration);
    drop(handle);
    drop(backend);
    assert!(!process_running(&pid));
    assert!(weak.upgrade().is_none());
    let backend = FirecrackerSandboxBackend::new(FirecrackerConfig {
        allow_guest_root: false,
        ..config
    })
    .await?;
    let error = backend
        .acquire_resolved(FirecrackerRequest {
            sandbox: request,
            egress_proxy: None,
        })
        .await
        .err()
        .context("external runtime replacement accepted")?;
    assert!(
        error
            .to_string()
            .contains("external disks require filesystem suspend")
    );
    assert!(
        backend
            .shared
            .load_machine_record(&id)
            .await?
            .context("allocation lost")?
            .external_disks
    );
    backend.terminate_all().await?;
    Ok(())
}
