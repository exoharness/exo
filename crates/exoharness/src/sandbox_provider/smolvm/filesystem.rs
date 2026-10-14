use super::super::capture_store;
use super::super::snapshot_lease::SnapshotLease;
use super::*;
use std::fs::{self, File};

const MAX_ALLOCATED: u64 = 64 * 1024 * 1024 * 1024;
const MAX_CAPTURE_MOUNTS: usize = 21;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Manifest {
    key: String,
    workdir: String,
    volumes: Vec<Volume>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Volume {
    guest_path: String,
    access: SandboxMountAccess,
    apfs: bool,
}

impl Manifest {
    pub(super) fn parse(payload: &SnapshotPayload) -> Result<Self> {
        ensure!(
            payload.format == SnapshotFormat::SmolvmFilesystemRef,
            "not a smolvm filesystem capture"
        );
        ensure!(
            payload.bytes.len() <= 1_048_576,
            "capture manifest exceeds its limit"
        );
        serde_json::from_slice::<Self>(&payload.bytes)?.validate()
    }

    fn validate(self) -> Result<Self> {
        ensure!(
            self.key.len() == 32 && self.key.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid smolvm capture identity"
        );
        ensure!(
            self.volumes.len() <= MAX_CAPTURE_MOUNTS,
            "too many captured volumes"
        );
        Ok(self)
    }
}

pub(super) struct Lease {
    pub directory: PathBuf,
    _lease: SnapshotLease,
}

pub(super) async fn lock_machine(root: &Path, machine: &str) -> Result<SnapshotLease> {
    // Slots can collide, and these file locks are not reentrant within a process.
    // Never hold more than one machine lock at a time, even for different machines.
    let directory = root.with_extension("locks");
    let slot = Sha256::digest(machine.as_bytes())[0];
    tokio::task::spawn_blocking(move || {
        fs::create_dir_all(&directory)?;
        capture_store::exclusive_lock(&directory.join(format!("{slot:02x}.lock")))
    })
    .await?
}

pub(super) fn staging_directory(root: &Path) -> Result<(tempfile::TempDir, SnapshotLease)> {
    capture_store::staging_directory(root, &root.join("budget.lock"))
}

pub(super) fn open(root: &Path, manifest: &Manifest) -> Result<Lease> {
    let directory = root.join(&manifest.key);
    let lease = capture_store::read_lease(&directory)?;
    let saved: Manifest = capture_store::read_manifest(&directory)?;
    ensure!(
        saved == *manifest,
        "capture manifest differs from published state"
    );
    ensure!(
        directory.join("machine.smolmachine").is_file(),
        "filesystem pack is missing"
    );
    for index in 0..manifest.volumes.len() {
        ensure!(
            directory.join(format!("volume-{index}")).is_dir(),
            "captured mount is missing"
        );
    }
    Ok(Lease {
        directory,
        _lease: lease,
    })
}

fn volume_root(path: &Path) -> Option<&Path> {
    let root = path.parent()?.parent()?;
    (path.ends_with("mount/workspace") && root.join("volume.sparseimage").is_file()).then_some(root)
}

fn enforce_budget(root: &Path, pending: u64) -> Result<()> {
    ensure!(
        capture_store::retained_allocation(root)?
            .checked_add(pending)
            .context("snapshot size overflow")?
            <= MAX_ALLOCATED,
        "retained filesystem capture budget exhausted"
    );
    Ok(())
}

pub(super) fn publish(
    root: &Path,
    staging: tempfile::TempDir,
    request: &SandboxRequest,
) -> Result<SnapshotPayload> {
    let mut volumes = Vec::new();
    ensure!(
        request.spec.mounts.len() <= MAX_CAPTURE_MOUNTS,
        "too many mounted volumes"
    );
    for (index, mount) in request.spec.mounts.iter().enumerate() {
        let target = staging.path().join(format!("volume-{index}"));
        let apfs = volume_root(&mount.host_path);
        if let Some(source) = apfs {
            fs::create_dir(&target)?;
            crate::local_volume::unmount_volume(source)?;
            let cloned = crate::local_volume::clone_volume(source, &target);
            crate::local_volume::mount_volume(source)?;
            cloned?;
        } else {
            crate::local_volume::clone_path(&mount.host_path, &target)?;
        }
        volumes.push(Volume {
            guest_path: mount.guest_path.clone(),
            access: mount.access,
            apfs: apfs.is_some(),
        });
    }
    let manifest = Manifest {
        key: uuid::Uuid::new_v4().simple().to_string(),
        workdir: request.spec.default_workdir.clone(),
        volumes,
    };
    let mut file = File::create(staging.path().join("manifest.json"))?;
    serde_json::to_writer(&mut file, &manifest)?;
    drop(file);
    capture_store::sync_tree(staging.path())?;
    let allocation = capture_store::record_allocation(staging.path())?;
    capture_store::publish(
        staging.path(),
        &root.join(&manifest.key),
        &root.join("budget.lock"),
        || enforce_budget(root, allocation),
    )?;
    Ok(SnapshotPayload {
        format: SnapshotFormat::SmolvmFilesystemRef,
        bytes: serde_json::to_vec(&manifest)?.into(),
    })
}

struct RestoredMount {
    target: PathBuf,
    backup: tempfile::TempDir,
    access: SandboxMountAccess,
    apfs: bool,
}

pub(super) struct RestoredMounts {
    mounts: Vec<RestoredMount>,
    committed: bool,
}

impl RestoredMounts {
    pub(super) fn keep(mut self) {
        self.committed = true;
    }
}

impl Drop for RestoredMounts {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for mount in self.mounts.drain(..).rev() {
            let cleanup = (|| {
                if mount.apfs {
                    crate::local_volume::unmount_volume(&mount.target)?;
                    let image = mount.target.join("volume.sparseimage");
                    if image.try_exists()? {
                        fs::remove_file(&image)?;
                    }
                    let previous = mount.backup.path().join("volume.sparseimage");
                    if previous.try_exists()? {
                        fs::rename(previous, &image)?;
                        mount_volume_with_access(&mount.target, mount.access)?;
                    } else if mount.target.join("mount").try_exists()? {
                        fs::remove_dir_all(mount.target.join("mount"))?;
                    }
                } else {
                    if mount.target.try_exists()? {
                        fs::remove_dir_all(&mount.target)?;
                    }
                    let previous = mount.backup.path().join("previous");
                    if previous.try_exists()? {
                        fs::rename(previous, &mount.target)?;
                    }
                }
                Ok::<_, anyhow::Error>(())
            })();
            if let Err(error) = cleanup {
                let backup = mount.backup.keep();
                tracing::warn!(path = %mount.target.display(), backup = %backup.display(), %error, "failed to roll back restored mount");
            }
        }
    }
}

fn mount_volume_with_access(target: &Path, access: SandboxMountAccess) -> Result<PathBuf> {
    if access == SandboxMountAccess::ReadOnly {
        crate::local_volume::mount_read_only(target)
    } else {
        crate::local_volume::mount_volume(target)
    }
}

pub(super) fn restore_mounts(
    directory: &Path,
    manifest: &Manifest,
    request: &mut SandboxRequest,
) -> Result<RestoredMounts> {
    ensure!(
        manifest.workdir == request.spec.default_workdir
            && manifest.volumes.len() == request.spec.mounts.len(),
        "filesystem capture layout changed"
    );
    let mut restored = RestoredMounts {
        mounts: Vec::new(),
        committed: false,
    };
    for (index, (volume, mount)) in manifest
        .volumes
        .iter()
        .zip(&mut request.spec.mounts)
        .enumerate()
    {
        ensure!(
            volume.guest_path == mount.guest_path && volume.access == mount.access,
            "mounted volume layout changed"
        );
        let source = directory.join(format!("volume-{index}"));
        let target = if volume.apfs {
            volume_root(&mount.host_path)
                .unwrap_or(&mount.host_path)
                .to_owned()
        } else {
            mount.host_path.clone()
        };
        let parent = target.parent().context("restored mount has no parent")?;
        fs::create_dir_all(parent)?;
        let backup = tempfile::Builder::new()
            .prefix(".restore-")
            .tempdir_in(parent)?;
        if volume.apfs {
            fs::create_dir_all(&target)?;
            crate::local_volume::unmount_volume(&target)?;
            let image = target.join("volume.sparseimage");
            if image.try_exists()?
                && let Err(error) = fs::rename(&image, backup.path().join("volume.sparseimage"))
            {
                return match mount_volume_with_access(&target, mount.access) {
                    Ok(_) => Err(error).context("staging previous resource volume"),
                    Err(cleanup) => Err(anyhow::Error::new(error)
                        .context(format!("remounting previous resource volume: {cleanup:#}"))),
                };
            }
        } else {
            crate::local_volume::clone_path(&source, &backup.path().join("new"))?;
            if target.try_exists()? {
                fs::rename(&target, backup.path().join("previous"))?;
            }
        }
        let staged = backup.path().join("new");
        restored.mounts.push(RestoredMount {
            target: target.clone(),
            backup,
            access: mount.access,
            apfs: volume.apfs,
        });
        if volume.apfs {
            crate::local_volume::clone_volume(&source, &target)?;
            mount.host_path = mount_volume_with_access(&target, mount.access)?;
        } else {
            fs::rename(staged, &target)?;
        }
    }
    Ok(restored)
}

pub(super) fn delete(root: &Path, payload: &SnapshotPayload) -> Result<()> {
    capture_store::delete(&root.join(Manifest::parse(payload)?.key))
}

pub(super) fn collect(root: &Path, keep: &[SnapshotPayload]) -> Result<usize> {
    let keep = keep
        .iter()
        .map(|payload| Ok(root.join(Manifest::parse(payload)?.key)))
        .collect::<Result<_>>()?;
    if !root.try_exists()? {
        return Ok(0);
    }
    let _budget = capture_store::exclusive_lock(&root.join("budget.lock"))?;
    capture_store::collect(root, &keep, |directory| {
        let manifest: Manifest = capture_store::read_manifest(directory)?;
        Ok(root.join(manifest.validate()?.key))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires APFS disk images"]
    fn managed_apfs_volume_restores_repeatedly_and_rolls_back_existing_volume() -> Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        fs::create_dir(&source)?;
        crate::local_volume::create_sized_volume(&source, 1)?;
        let workspace = crate::local_volume::mount_volume(&source)?;
        let (mut request, payload) =
            capture_fixture(root.path(), vec![mount(&workspace, "/repo")])?;
        fs::write(workspace.join("marker"), b"later")?;
        let manifest = Manifest::parse(&payload)?;
        let lease = open(root.path(), &manifest)?;
        let target = root.path().join("target");
        fs::create_dir(&target)?;
        crate::local_volume::create_sized_volume(&target, 1)?;
        let target_workspace = crate::local_volume::mount_volume(&target)?;
        request.spec.mounts[0].host_path = target_workspace.clone();
        for _ in 0..2 {
            fs::write(target_workspace.join("marker"), b"changed")?;
            let mounts = restore_mounts(&lease.directory, &manifest, &mut request)?;
            assert_eq!(request.spec.mounts[0].host_path, target_workspace);
            assert_eq!(fs::read(target_workspace.join("marker"))?, b"captured");
            mounts.keep();
        }
        request.spec.mounts[0].host_path = workspace.clone();
        request
            .spec
            .mounts
            .push(mount(&root.path().join("missing"), "/missing"));
        let mut partial = manifest.clone();
        let mut missing = manifest.volumes[0].clone();
        missing.guest_path = "/missing".into();
        partial.volumes.push(missing);
        assert!(restore_mounts(&lease.directory, &partial, &mut request).is_err());
        assert_eq!(fs::read(workspace.join("marker"))?, b"later");
        crate::local_volume::unmount_volume(&source)?;
        crate::local_volume::unmount_volume(&target)?;
        drop(lease);
        delete(root.path(), &payload)?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn directory_captures_restore_independently_and_hold_deletion_leases() -> Result<()> {
        use std::io::Read;
        let root = tempfile::tempdir()?;
        let workspace = root.path().join("source");
        let (mut request, payload) =
            capture_fixture(root.path(), vec![mount(&workspace, "/repo")])?;
        let manifest = Manifest::parse(&payload)?;
        let lease = open(root.path(), &manifest)?;
        let peer = request.clone();
        fs::write(workspace.join("marker"), b"later")?;
        let mut attached = File::open(peer.spec.mounts[0].host_path.join("marker"))?;
        let mut first = request.clone();
        first.spec.mounts[0].host_path = root.path().join("first");
        restore_mounts(&lease.directory, &manifest, &mut first)?.keep();
        fs::write(first.spec.mounts[0].host_path.join("marker"), b"edited")?;
        let mut second = request.clone();
        second.spec.mounts[0].host_path = root.path().join("second");
        restore_mounts(&lease.directory, &manifest, &mut second)?.keep();
        assert_eq!(
            fs::read(second.spec.mounts[0].host_path.join("marker"))?,
            b"captured"
        );
        restore_mounts(&lease.directory, &manifest, &mut request)?.keep();
        assert_eq!(request.spec.mounts[0].host_path, workspace);
        assert_eq!(
            fs::read(peer.spec.mounts[0].host_path.join("marker"))?,
            b"captured"
        );
        let mut previous = Vec::new();
        attached.read_to_end(&mut previous)?;
        assert_eq!(previous, b"later");
        assert!(delete(root.path(), &payload).is_err());
        drop(lease);
        delete(root.path(), &payload)?;
        delete(root.path(), &payload)?;
        Ok(())
    }

    #[tokio::test]
    async fn machine_lock_files_are_bounded_and_reused() -> Result<()> {
        let root = tempfile::tempdir()?;
        let captures = root.path().join("captures");
        let mut slots = std::collections::HashSet::new();
        for index in 0..512 {
            let machine = format!("machine-{index}");
            slots.insert(Sha256::digest(machine.as_bytes())[0]);
            drop(lock_machine(&captures, &machine).await?);
        }
        let count = fs::read_dir(captures.with_extension("locks"))?.count();
        assert_eq!(count, slots.len());
        assert!(count <= 256);
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn cancelled_restore_keeps_mounts_and_leases_until_start_finishes() -> Result<()> {
        for succeeds in [true, false] {
            let root = tempfile::tempdir()?;
            let (backend, request, payload) = restore_fixture(
                root.path(),
                &format!(
                    r#"
root=$(dirname "$0")
case "$1 $2" in
  '--version ') printf 'smolvm 1.20.0\n';;
  'machine status') printf 'machine not found\n' >&2; exit 1;;
  'machine start')
    touch "$root/starting"
    attempts=0
    until [ -f "$root/release" ]; do
      attempts=$((attempts + 1))
      [ "$attempts" -lt 500 ] || exit 24
      sleep 0.01
    done
    [ "{succeeds}" = true ] || {{ printf 'start failure\n' >&2; exit 23; }};;
  *) exit 0;;
esac"#
                ),
            )?;
            let target = request.spec.mounts[0].host_path.clone();
            let restoring = {
                let backend = backend.clone();
                let request = request.clone();
                let payload = payload.clone();
                tokio::spawn(async move { backend.acquire_from_snapshot(request, payload).await })
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                while !root.path().join("starting").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            restoring.abort();
            assert!(
                restoring
                    .await
                    .err()
                    .context("caller should be cancelled")?
                    .is_cancelled()
            );
            assert_eq!(fs::read(target.join("marker"))?, b"captured");
            assert!(backend.delete_snapshot(payload.clone()).await.is_err());
            let stopping = {
                let backend = backend.clone();
                let request = request.clone();
                tokio::spawn(async move { backend.stop(request).await })
            };
            tokio::time::sleep(Duration::from_millis(30)).await;
            let waiting = !stopping.is_finished();
            fs::write(root.path().join("release"), b"resume")?;
            tokio::time::timeout(Duration::from_secs(5), stopping).await???;
            assert!(waiting, "restore released its machine lock during start");
            assert_eq!(target.join("marker").exists(), succeeds);
            assert_eq!(target.join("existing").exists(), !succeeds);
            backend.delete_snapshot(payload).await?;
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn mount(path: &Path, guest: &str) -> crate::SandboxMount {
        crate::SandboxMount {
            host_path: path.to_owned(),
            guest_path: guest.into(),
            access: SandboxMountAccess::ReadWrite,
            internal: false,
        }
    }

    fn capture_request(mounts: Vec<crate::SandboxMount>) -> Result<crate::SandboxRequest> {
        for mount in &mounts {
            fs::create_dir_all(&mount.host_path)?;
            fs::write(mount.host_path.join("marker"), b"captured")?;
        }
        let mut request = super::super::tests::test_request(Some(Duration::from_secs(60)));
        request.spec.mounts = mounts;
        Ok(request)
    }

    fn staged_pack(root: &Path) -> Result<(tempfile::TempDir, SnapshotLease)> {
        let (staging, lease) = staging_directory(root)?;
        fs::write(
            staging.path().join("machine.smolmachine"),
            b"unit-test pack",
        )?;
        Ok((staging, lease))
    }

    fn capture_fixture(
        root: &Path,
        mounts: Vec<crate::SandboxMount>,
    ) -> Result<(crate::SandboxRequest, SnapshotPayload)> {
        let request = capture_request(mounts)?;
        let (staging, _lease) = staged_pack(root)?;
        let payload = publish(root, staging, &request)?;
        Ok((request, payload))
    }

    #[cfg(target_os = "macos")]
    fn restore_fixture(
        root: &Path,
        script: &str,
    ) -> Result<(SmolvmSandboxBackend, crate::SandboxRequest, SnapshotPayload)> {
        let binary = root.join("smolvm");
        super::super::tests::write_test_binary(&binary, script);
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            binary: Some(binary),
            mode: SmolvmExecutionMode::Warm,
            snapshot_root: Some(root.join("captures")),
            ..Default::default()
        });
        let (mut request, payload) = capture_fixture(
            &backend.snapshot_root,
            vec![mount(&root.join("source"), "/repo")],
        )?;
        let target = root.join("target");
        fs::create_dir(&target)?;
        fs::write(target.join("existing"), b"previous")?;
        request.spec.mounts[0].host_path = target;
        Ok((backend, request, payload))
    }

    #[test]
    fn cached_allocations_enforce_the_budget_and_deletion_frees_it() -> Result<()> {
        let root = tempfile::tempdir()?;
        let payload = capture_fixture(root.path(), vec![])?.1;
        let directory = root.path().join(Manifest::parse(&payload)?.key);
        assert_eq!(
            capture_store::retained_allocation(root.path())?,
            capture_store::allocated(&directory)?
        );
        fs::write(directory.join("allocated.bin"), MAX_ALLOCATED.to_le_bytes())?;
        let error = capture_fixture(root.path(), vec![]).unwrap_err();
        assert!(error.to_string().contains("budget exhausted"));
        delete(root.path(), &payload)?;
        assert_eq!(capture_store::retained_allocation(root.path())?, 0);
        capture_fixture(root.path(), vec![])?;
        Ok(())
    }

    #[tokio::test]
    async fn filesystem_snapshot_allocated_bytes_uses_cached_total_and_rejects_missing_captures()
    -> Result<()> {
        let root = tempfile::tempdir()?;
        let backend = SmolvmSandboxBackend {
            snapshot_root: root.path().into(),
            ..SmolvmSandboxBackend::new()
        };
        let (staging, staging_lease) = staged_pack(root.path())?;
        File::options()
            .write(true)
            .open(staging.path().join("machine.smolmachine"))?
            .set_len(1024 * 1024 * 1024)?;
        let payload = publish(root.path(), staging, &capture_request(vec![])?)?;
        drop(staging_lease);
        let lease = open(root.path(), &Manifest::parse(&payload)?)?;
        let expected = capture_store::allocated(&lease.directory)?;
        fs::write(lease.directory.join("unaccounted"), vec![1; 1024 * 1024])?;
        let bytes = backend
            .filesystem_snapshot_allocated_bytes(payload.clone())
            .await?;
        assert_eq!(bytes, expected);
        assert!(bytes > 0 && bytes < 1024 * 1024);
        fs::write(lease.directory.join("allocated.bin"), b"invalid")?;
        assert!(
            backend
                .filesystem_snapshot_allocated_bytes(payload.clone())
                .await
                .is_err()
        );
        drop(lease);
        backend.delete_snapshot(payload.clone()).await?;
        assert!(
            backend
                .filesystem_snapshot_allocated_bytes(payload)
                .await
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn collection_validates_manifest_keys_and_preserves_cataloged_captures() -> Result<()> {
        let root = tempfile::tempdir()?;
        let kept = capture_fixture(root.path(), vec![])?.1;
        let invalid = capture_fixture(root.path(), vec![])?.1;
        let mut manifest = Manifest::parse(&invalid)?;
        let directory = root.path().join(&manifest.key);
        manifest.key = "../outside".into();
        fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec(&manifest)?,
        )?;
        assert_eq!(collect(root.path(), std::slice::from_ref(&kept))?, 0);
        assert!(directory.exists());
        assert!(open(root.path(), &Manifest::parse(&kept)?).is_ok());
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn capture_preparation_does_not_wait_for_the_store_lock() -> Result<()> {
        let root = tempfile::tempdir()?;
        let captures = root.path().join("captures");
        let source = root.path().join("source");
        let request = capture_request(vec![mount(&source, "/repo")])?;
        let (staging, staging_lease) = staged_pack(&captures)?;
        let directory = staging.path().to_owned();
        let budget = capture_store::exclusive_lock(&captures.join("budget.lock"))?;
        let root_for_publish = captures.clone();
        let publisher = std::thread::spawn(move || {
            let _lease = staging_lease;
            publish(&root_for_publish, staging, &request)
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !directory.join("manifest.json").is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let prepared = directory.join("manifest.json").is_file();
        let copied = fs::read(directory.join("volume-0/marker"));
        drop(budget);
        let payload = publisher.join().expect("capture publisher panicked")?;
        assert!(prepared, "capture preparation waited for the store lock");
        assert_eq!(copied?, b"captured");
        assert!(open(&captures, &Manifest::parse(&payload)?).is_ok());
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn partial_mount_restore_removes_its_copies_and_preserves_existing_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (mut request, payload) = capture_fixture(
            root.path(),
            vec![
                mount(&root.path().join("source-first"), "/first"),
                mount(&root.path().join("source-second"), "/second"),
            ],
        )?;
        let manifest = Manifest::parse(&payload)?;
        let lease = open(root.path(), &manifest)?;
        fs::remove_dir_all(lease.directory.join("volume-1"))?;
        let first = root.path().join("first");
        fs::create_dir(&first)?;
        fs::write(first.join("existing"), b"keep")?;
        request.spec.mounts[0].host_path = first.clone();
        request.spec.mounts[1].host_path = root.path().join("second");
        assert!(restore_mounts(&lease.directory, &manifest, &mut request).is_err());
        assert!(!first.join("marker").exists());
        assert_eq!(fs::read(first.join("existing"))?, b"keep");
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn failed_vm_restore_removes_mount_copies_after_create_or_start_failure() -> Result<()> {
        for (failure, cleanup_fails) in [
            ("create", false),
            ("start", false),
            ("create", true),
            ("start", true),
        ] {
            let root = tempfile::tempdir()?;
            let delete = if cleanup_fails {
                "printf 'cleanup failure\\n' >&2; exit 24"
            } else {
                "exit 0"
            };
            let (backend, request, payload) = restore_fixture(
                root.path(),
                &format!(
                    r#"
case "$1 $2" in
  '--version ') printf 'smolvm 1.20.0\n';;
  'machine status') printf 'machine not found\n' >&2; exit 1;;
  'machine create')
    printf '%s\n' "$@" > "$(dirname "$0")/create-args"
    [ "{failure}" != create ] || {{ printf 'injected failure\n' >&2; exit 23; }};;
  'machine start')
    [ "{failure}" != start ] || {{ printf 'injected failure\n' >&2; exit 23; }};;
  'machine delete') {delete};;
  *) exit 0;;
esac"#
                ),
            )?;
            let target = request.spec.mounts[0].host_path.clone();
            let error = backend
                .acquire_from_snapshot(request, payload)
                .await
                .err()
                .context("restore should fail")?;
            let args = fs::read_to_string(root.path().join("create-args"))?;
            assert!(args.contains("--from\n"), "{args}");
            assert!(!args.contains("--image\n"), "{args}");
            assert!(
                args.contains("--\n/bin/sh\n-c\ntrap 'kill -TERM -1; wait; exit 0' INT TERM;"),
                "{args}"
            );
            let message = format!("{error:#}");
            assert!(message.contains("injected failure"), "{message}");
            assert_eq!(
                message.contains("cleanup failure"),
                cleanup_fails && failure == "start",
                "{message}"
            );
            assert!(!target.join("marker").exists());
            assert_eq!(fs::read(target.join("existing"))?, b"previous");
        }
        Ok(())
    }
}
