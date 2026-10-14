use super::snapshot_lease::SnapshotLease;
use anyhow::{Context, Result, bail, ensure};
use serde::de::DeserializeOwned;
use std::collections::HashSet;
use std::fs::{self, File, TryLockError};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub(super) fn exclusive_lock(path: &Path) -> Result<SnapshotLease> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    Ok(SnapshotLease::exclusive(file)?)
}

pub(super) fn staging_directory(
    parent: &Path,
    budget_file: &Path,
) -> Result<(tempfile::TempDir, SnapshotLease)> {
    fs::create_dir_all(parent)?;
    let _budget = exclusive_lock(budget_file)?;
    let directory = tempfile::Builder::new()
        .prefix(".capture-")
        .tempdir_in(parent)?;
    let file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.path().join("lease"))?;
    Ok((directory, SnapshotLease::exclusive(file)?))
}

pub(super) fn read_lease(directory: &Path) -> Result<SnapshotLease> {
    Ok(SnapshotLease::shared(
        File::open(directory.join("lease"))
            .context("filesystem capture is missing on this host")?,
    )?)
}

pub(super) fn read_manifest<M: DeserializeOwned>(directory: &Path) -> Result<M> {
    let file = File::open(directory.join("manifest.json"))?;
    ensure!(
        file.metadata()?.len() <= 1_048_576,
        "capture manifest exceeds its limit"
    );
    Ok(serde_json::from_reader(file)?)
}

pub(super) fn directories(root: &Path) -> Result<impl Iterator<Item = Result<PathBuf>>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    Ok(entries.into_iter().flatten().filter_map(|entry| {
        let directory = (|| {
            let entry = entry?;
            Ok::<_, std::io::Error>(entry.file_type()?.is_dir().then(|| entry.path()))
        })();
        match directory {
            Ok(directory) => directory.map(Ok),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(Err(error.into())),
        }
    }))
}

pub(super) fn delete(directory: &Path) -> Result<()> {
    ensure!(
        try_delete(directory)? || !directory.try_exists()?,
        "filesystem capture is in use"
    );
    Ok(())
}

fn try_delete(directory: &Path) -> Result<bool> {
    let file = match File::open(directory.join("lease")) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let _lease = match file.map(SnapshotLease::try_exclusive).transpose() {
        Ok(lease) => lease,
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    };
    match fs::remove_dir_all(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    File::open(directory.parent().context("missing capture parent")?)?.sync_all()?;
    Ok(true)
}

pub(super) fn collect(
    root: &Path,
    keep: &HashSet<PathBuf>,
    directory_for_manifest: impl Fn(&Path) -> Result<PathBuf>,
) -> Result<usize> {
    let mut removed = 0;
    for directory in directories(root)? {
        let directory = directory?;
        if directory
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".capture-"))
        {
            removed += usize::from(try_delete(&directory)?);
            continue;
        }
        let valid = directory_for_manifest(&directory).and_then(|expected| {
            ensure!(
                expected == directory,
                "capture directory differs from manifest"
            );
            Ok(())
        });
        match valid {
            Ok(()) if !keep.contains(&directory) => removed += usize::from(try_delete(&directory)?),
            Ok(()) => {}
            Err(error) => {
                tracing::warn!(path = %directory.display(), %error, "skipping invalid filesystem capture")
            }
        }
    }
    Ok(removed)
}

pub(super) fn allocated(root: &Path) -> Result<u64> {
    walk_allocated(root, &mut HashSet::new(), false)
}

#[cfg(feature = "firecracker")]
pub(super) fn allocated_files(root: &Path, inodes: &mut HashSet<(u64, u64)>) -> Result<u64> {
    walk_allocated(root, inodes, true)
}

fn walk_allocated(root: &Path, inodes: &mut HashSet<(u64, u64)>, files_only: bool) -> Result<u64> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if files_only && !metadata.is_dir() && !metadata.is_file() {
        bail!("unexpected file in snapshot storage");
    }
    let mut bytes =
        if (!files_only || metadata.is_file()) && inodes.insert((metadata.dev(), metadata.ino())) {
            metadata
                .blocks()
                .checked_mul(512)
                .context("snapshot size overflow")?
        } else {
            0
        };
    if metadata.is_dir() {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            bytes = bytes
                .checked_add(walk_allocated(&entry.path(), inodes, files_only)?)
                .context("snapshot size overflow")?;
        }
    }
    Ok(bytes)
}

pub(super) fn record_allocation(directory: &Path) -> Result<u64> {
    let path = directory.join("allocated.bin");
    fs::write(&path, 0u64.to_le_bytes())?;
    File::open(&path)?.sync_all()?;
    let bytes = allocated(directory)?;
    fs::write(&path, bytes.to_le_bytes())?;
    File::open(path)?.sync_all()?;
    Ok(bytes)
}

pub(super) fn cached_allocation(directory: &Path) -> std::io::Result<u64> {
    let allocation: [u8; 8] = fs::read(directory.join("allocated.bin"))?
        .try_into()
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid capture allocation total",
            )
        })?;
    Ok(u64::from_le_bytes(allocation))
}

pub(super) fn retained_allocation(root: &Path) -> Result<u64> {
    let mut bytes = 0u64;
    for directory in directories(root)? {
        let directory = directory?;
        if directory
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".capture-"))
        {
            continue;
        }
        let allocation = match cached_allocation(&directory) {
            Ok(allocation) => allocation,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        bytes = bytes
            .checked_add(allocation)
            .context("snapshot size overflow")?;
    }
    Ok(bytes)
}

pub(super) fn sync_tree(root: &Path) -> Result<()> {
    let mut count = 0;
    for entry in fs::read_dir(root)? {
        sync_node(&entry?.path(), &mut count)?;
    }
    Ok(())
}

fn sync_node(root: &Path, count: &mut usize) -> Result<()> {
    *count += 1;
    ensure!(*count <= 262_144, "capture file count exceeds its limit");
    let metadata = fs::symlink_metadata(root)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(root)? {
            sync_node(&entry?.path(), count)?;
        }
    }
    if metadata.is_file() || metadata.is_dir() {
        File::open(root)?.sync_all()?;
    }
    Ok(())
}

pub(super) fn publish(
    temporary: &Path,
    destination: &Path,
    budget_file: &Path,
    check_budget: impl FnOnce() -> Result<()>,
) -> Result<()> {
    File::open(temporary)?.sync_all()?;
    {
        let _budget = exclusive_lock(budget_file)?;
        check_budget()?;
        fs::rename(temporary, destination)
            .with_context(|| format!("publishing capture {}", destination.display()))?;
    }
    let parent = destination.parent().context("missing capture parent")?;
    File::open(parent)?.sync_all()?;
    if let Some(namespace) = parent.parent().filter(|path| !path.as_os_str().is_empty()) {
        File::open(namespace)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn syncing_a_capture_skips_named_pipes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let fifo = root.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()?
                .success()
        );
        fs::write(root.path().join("regular"), b"state")?;
        let directory = root.path().to_owned();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || sender.send(sync_tree(&directory)).unwrap());
        let result = receiver.recv_timeout(std::time::Duration::from_secs(5));
        if result.is_err() {
            let _writer = File::options()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)?;
        }
        worker.join().expect("capture sync panicked");
        result.context("capture sync blocked on a FIFO")??;
        Ok(())
    }

    #[test]
    fn retained_allocation_reads_totals_without_walking_historical_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        let capture = root.path().join("retained");
        let volume = capture.join("volume");
        fs::create_dir_all(&volume)?;
        fs::write(volume.join("file"), b"state")?;
        let expected = record_allocation(&capture)?;
        let (pending, _lease) = staging_directory(root.path(), &root.path().join("budget.lock"))?;
        fs::write(pending.path().join("allocated.bin"), u64::MAX.to_le_bytes())?;
        fs::set_permissions(&volume, std::os::unix::fs::PermissionsExt::from_mode(0o000))?;
        let retained = retained_allocation(root.path());
        fs::set_permissions(&volume, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        assert_eq!(retained?, expected);
        fs::remove_dir_all(capture)?;
        assert_eq!(retained_allocation(root.path())?, 0);
        Ok(())
    }

    #[test]
    fn collection_skips_busy_and_corrupt_captures_and_reclaims_abandoned_staging() -> Result<()> {
        let root = tempfile::tempdir()?;
        for key in ["kept", "busy", "stale"] {
            let directory = root.path().join(key);
            fs::create_dir(&directory)?;
            File::create(directory.join("lease"))?;
            fs::write(directory.join("manifest.json"), serde_json::to_vec(key)?)?;
        }
        let kept = root.path().join("kept");
        let busy = root.path().join("busy");
        let stale = root.path().join("stale");
        let busy_lease = read_lease(&busy)?;
        let (active, active_lease) =
            staging_directory(root.path(), &root.path().join("budget.lock"))?;
        assert_eq!(
            fs::metadata(active.path().join("lease"))?.mode() & 0o777,
            0o600
        );
        let incomplete = root.path().join(".capture-incomplete");
        fs::create_dir(&incomplete)?;
        fs::write(incomplete.join("data"), b"incomplete")?;
        let unpublished = root.path().join(".capture-unpublished");
        fs::create_dir(&unpublished)?;
        fs::write(unpublished.join("manifest.json"), b"\"unpublished\"")?;
        let corrupt = root.path().join("corrupt");
        fs::create_dir(&corrupt)?;
        fs::write(corrupt.join("manifest.json"), b"invalid JSON")?;
        let keep = HashSet::from([kept.clone()]);
        let directory_for =
            |directory: &Path| Ok(root.path().join(read_manifest::<String>(directory)?));
        assert!(delete(&busy).is_err());
        assert_eq!(collect(root.path(), &keep, directory_for)?, 3);
        assert!(busy.exists());
        assert!(active.path().exists());
        assert!(corrupt.exists());
        assert!(!stale.exists());
        assert!(!incomplete.exists());
        assert!(!unpublished.exists());
        drop(busy_lease);
        drop(active_lease);
        assert_eq!(collect(root.path(), &keep, directory_for)?, 2);
        assert!(read_lease(&kept).is_ok());
        delete(&kept)?;
        delete(&kept)?;
        Ok(())
    }

    #[test]
    fn allocated_walk_tolerates_concurrent_directory_removal() -> Result<()> {
        let root = tempfile::tempdir()?;
        for round in 0..8 {
            let capture = root.path().join("capture");
            fs::create_dir(&capture)?;
            for index in 0..128 {
                let directory = capture.join(index.to_string());
                fs::create_dir(&directory)?;
                fs::write(directory.join("disk"), b"data")?;
            }
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                let delete = scope.spawn(|| {
                    barrier.wait();
                    fs::remove_dir_all(&capture)
                });
                barrier.wait();
                let walked = if round % 2 == 0 {
                    allocated(root.path())
                } else {
                    walk_allocated(root.path(), &mut HashSet::new(), true)
                };
                delete.join().expect("capture deletion panicked")?;
                walked?;
                Ok::<_, anyhow::Error>(())
            })?;
        }
        Ok(())
    }
}
