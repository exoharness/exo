use super::*;
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

const MAX_ALLOCATED: u64 = 64 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Manifest {
    version: u32,
    key: String,
    workdir: String,
    volumes: Vec<Volume>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Volume {
    guest_path: String,
    access: SandboxMountAccess,
    apfs: bool,
}

impl Manifest {
    pub(super) fn parse(payload: &SnapshotPayload) -> Result<Self> {
        ensure!(
            payload.format == SnapshotFormat::SmolvmMachinePack,
            "not a smolvm filesystem capture"
        );
        ensure!(
            payload.bytes.len() <= 1_048_576,
            "capture manifest exceeds its limit"
        );
        let manifest: Self = serde_json::from_slice(&payload.bytes)?;
        ensure!(
            manifest.version == 1
                && manifest.key.len() == 32
                && manifest.key.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid smolvm capture identity"
        );
        ensure!(manifest.volumes.len() <= 21, "too many captured volumes");
        Ok(manifest)
    }
}

pub(super) struct Lease {
    pub directory: PathBuf,
    _file: File,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Err(error) = lock(&self._file, libc::LOCK_UN) {
            tracing::warn!(%error, "failed to release filesystem capture lease");
        }
    }
}

fn lock(file: &File, operation: i32) -> Result<()> {
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), operation) } == 0,
        "filesystem capture is in use: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

pub(super) fn open(root: &Path, manifest: &Manifest) -> Result<Lease> {
    let directory = root.join(&manifest.key);
    let file = File::open(directory.join("lease"))?;
    lock(&file, libc::LOCK_SH)?;
    let saved: Manifest = serde_json::from_reader(File::open(directory.join("manifest.json"))?)?;
    ensure!(
        serde_json::to_vec(&saved)? == serde_json::to_vec(manifest)?,
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
        _file: file,
    })
}

pub(super) fn allocated(root: &Path) -> Result<u64> {
    fn walk(root: &Path, count: &mut usize) -> Result<u64> {
        let metadata = fs::symlink_metadata(root)?;
        *count += 1;
        ensure!(*count <= 262_144, "capture file count exceeds its limit");
        let mut bytes = metadata.blocks() * 512;
        if metadata.is_dir() {
            for entry in fs::read_dir(root)? {
                bytes += walk(&entry?.path(), count)?;
            }
        }
        Ok(bytes)
    }
    walk(root, &mut 0)
}

fn clone_directory(source: &Path, target: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let source = CString::new(source.as_os_str().as_bytes())?;
        let target = CString::new(target.as_os_str().as_bytes())?;
        ensure!(
            unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) } == 0,
            "mounted directory copies require APFS: {}",
            std::io::Error::last_os_error()
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        let output = std::process::Command::new("cp")
            .args(["-a", "--reflink=always", "--"])
            .arg(source)
            .arg(target)
            .output()?;
        ensure!(
            output.status.success(),
            "mounted directory copies require reflinks: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn volume_root(path: &Path) -> Option<&Path> {
    let root = path.parent()?.parent()?;
    (path.ends_with("mount/workspace") && root.join("volume.sparseimage").is_file()).then_some(root)
}

pub(super) fn publish(
    root: &Path,
    staging: tempfile::TempDir,
    request: &SandboxRequest,
) -> Result<SnapshotPayload> {
    let budget = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("budget.lock"))?;
    lock(&budget, libc::LOCK_EX)?;
    let mut volumes = Vec::new();
    ensure!(request.spec.mounts.len() <= 21, "too many mounted volumes");
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
            clone_directory(&mount.host_path, &target)?;
        }
        volumes.push(Volume {
            guest_path: mount.guest_path.clone(),
            access: mount.access,
            apfs: apfs.is_some(),
        });
    }
    ensure!(
        allocated(root)? <= MAX_ALLOCATED,
        "retained filesystem capture budget exhausted"
    );
    let manifest = Manifest {
        version: 1,
        key: uuid::Uuid::new_v4().simple().to_string(),
        workdir: request.spec.default_workdir.clone(),
        volumes,
    };
    File::create(staging.path().join("lease"))?.sync_all()?;
    let mut file = File::create(staging.path().join("manifest.json"))?;
    serde_json::to_writer(&mut file, &manifest)?;
    file.sync_all()?;
    sync_tree(staging.path())?;
    let destination = root.join(&manifest.key);
    fs::rename(staging.path(), destination)?;
    File::open(root)?.sync_all()?;
    Ok(SnapshotPayload {
        format: SnapshotFormat::SmolvmMachinePack,
        bytes: serde_json::to_vec(&manifest)?.into(),
    })
}

fn sync_tree(root: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(root)? {
            sync_tree(&entry?.path())?;
        }
    }
    if !metadata.file_type().is_symlink() {
        File::open(root)?.sync_all()?;
    }
    Ok(())
}

pub(super) fn restore_mounts(
    directory: &Path,
    manifest: &Manifest,
    request: &mut SandboxRequest,
) -> Result<()> {
    ensure!(
        manifest.workdir == request.spec.default_workdir
            && manifest.volumes.len() == request.spec.mounts.len(),
        "filesystem capture layout changed"
    );
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
        fs::create_dir_all(&mount.host_path)?;
        if volume.apfs {
            crate::local_volume::clone_volume(&source, &mount.host_path)?;
            mount.host_path = crate::local_volume::mount_volume(&mount.host_path)?;
        } else {
            let target = mount.host_path.join("captured");
            clone_directory(&source, &target)?;
            mount.host_path = target;
        }
    }
    Ok(())
}

pub(super) fn delete(root: &Path, payload: &SnapshotPayload) -> Result<()> {
    let manifest = Manifest::parse(payload)?;
    let directory = root.join(manifest.key);
    if !directory.try_exists()? {
        return Ok(());
    }
    let file = File::open(directory.join("lease"))?;
    lock(&file, libc::LOCK_EX | libc::LOCK_NB)?;
    fs::remove_dir_all(directory)?;
    File::open(root)?.sync_all()?;
    Ok(())
}

pub(super) fn collect(root: &Path, keep: &[SnapshotPayload]) -> Result<usize> {
    let keep = keep
        .iter()
        .map(Manifest::parse)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|manifest| manifest.key)
        .collect::<BTreeSet<_>>();
    if !root.exists() {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join("manifest.json");
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(path)?;
        let payload = SnapshotPayload {
            format: SnapshotFormat::SmolvmMachinePack,
            bytes: bytes.into(),
        };
        let manifest = Manifest::parse(&payload)?;
        if !keep.contains(&manifest.key) {
            delete(root, &payload)?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires APFS disk images"]
    fn managed_apfs_volume_capture_remounts_source_and_restores_fresh_volume() -> Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        fs::create_dir(&source)?;
        crate::local_volume::create_sized_volume(&source, 1)?;
        let workspace = crate::local_volume::mount_volume(&source)?;
        fs::write(workspace.join("marker"), b"captured")?;
        let mut request = super::super::tests::test_request(Some(Duration::from_secs(60)));
        request.spec.mounts = vec![crate::SandboxMount {
            host_path: workspace.clone(),
            guest_path: "/repo".into(),
            access: SandboxMountAccess::ReadWrite,
            internal: false,
        }];
        let staging = tempfile::tempdir_in(root.path())?;
        fs::write(
            staging.path().join("machine.smolmachine"),
            b"unit-test pack",
        )?;
        let payload = publish(root.path(), staging, &request)?;
        fs::write(workspace.join("marker"), b"later")?;
        let manifest = Manifest::parse(&payload)?;
        let lease = open(root.path(), &manifest)?;
        let target = root.path().join("target");
        request.spec.mounts[0].host_path = target.clone();
        let result = restore_mounts(&lease.directory, &manifest, &mut request);
        let captured =
            result.and_then(|_| Ok(fs::read(request.spec.mounts[0].host_path.join("marker"))?));
        crate::local_volume::unmount_volume(&source)?;
        crate::local_volume::unmount_volume(&target)?;
        assert_eq!(captured?, b"captured");
        drop(lease);
        delete(root.path(), &payload)?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn directory_captures_restore_independently_and_hold_deletion_leases() -> Result<()> {
        let root = tempfile::tempdir()?;
        let workspace = root.path().join("source");
        fs::create_dir(&workspace)?;
        fs::write(workspace.join("marker"), b"original")?;
        let staging = tempfile::tempdir_in(root.path())?;
        fs::write(
            staging.path().join("machine.smolmachine"),
            b"unit-test pack",
        )?;
        let mut request = super::super::tests::test_request(Some(Duration::from_secs(60)));
        request.spec.mounts = vec![crate::SandboxMount {
            host_path: workspace.clone(),
            guest_path: "/repo".into(),
            access: SandboxMountAccess::ReadWrite,
            internal: false,
        }];
        let payload = publish(root.path(), staging, &request)?;
        let manifest = Manifest::parse(&payload)?;
        let lease = open(root.path(), &manifest)?;
        fs::write(workspace.join("marker"), b"later")?;
        let mut first = request.clone();
        first.spec.mounts[0].host_path = root.path().join("first");
        restore_mounts(&lease.directory, &manifest, &mut first)?;
        fs::write(first.spec.mounts[0].host_path.join("marker"), b"edited")?;
        let mut second = request;
        second.spec.mounts[0].host_path = root.path().join("second");
        restore_mounts(&lease.directory, &manifest, &mut second)?;
        assert_eq!(
            fs::read(second.spec.mounts[0].host_path.join("marker"))?,
            b"original"
        );
        assert!(delete(root.path(), &payload).is_err());
        drop(lease);
        delete(root.path(), &payload)?;
        delete(root.path(), &payload)?;
        Ok(())
    }
}
