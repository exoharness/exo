use super::*;
use std::os::unix::fs::OpenOptionsExt;

#[derive(Deserialize)]
struct Mounts {
    filesystems: Vec<Mount>,
}

#[derive(Deserialize)]
struct Mount {
    target: PathBuf,
    fstype: String,
}

pub(in crate::sandbox_provider) fn prepare(config: &FirecrackerConfig) -> Result<()> {
    let root = &config.state_root;
    fs::create_dir_all(root)?;
    fs::set_permissions(root, Permissions::from_mode(0o700))?;
    let root = fs::canonicalize(root)?;
    validate_private_root(&root)?;
    let _setup_lock = lock(&PathBuf::from(format!("{}.storage.lock", path(&root)?)))?;
    let mounts: Mounts = serde_json::from_slice(&checked(
        "findmnt",
        &["--json", "--list", "--output", "TARGET,FSTYPE"],
    )?)?;
    let filesystem = mounts
        .filesystems
        .iter()
        .filter(|mount| root.starts_with(&mount.target))
        .max_by_key(|mount| mount.target.components().count())
        .context("locating the Firecracker state filesystem")?;
    if filesystem.fstype == "xfs" {
        return Ok(());
    }
    ensure!(
        !mounts
            .filesystems
            .iter()
            .any(|mount| mount.target.starts_with(&root)),
        "cannot provision XFS over an existing mount at {}",
        root.display()
    );
    let _backend_lock = lock(&root.join("backend.lock"))
        .context("stop the active Firecracker backend before provisioning XFS storage")?;
    let jails = jail_dir(config, "");
    if jails.try_exists()? {
        for entry in fs::read_dir(jails)? {
            ensure!(
                !process_running(&entry?.path().join("root/firecracker.pid")),
                "stop running Firecracker sandboxes before provisioning XFS storage"
            );
        }
    }
    let image = PathBuf::from(format!("{}.xfs", path(&root)?));
    if !image.try_exists()? {
        tracing::info!(path = %root.display(), "Preparing Firecracker XFS storage");
        let parent = root
            .parent()
            .context("Firecracker state root has no parent")?;
        let space = rustix::fs::statvfs(parent)?;
        let capacity = space.f_bavail.saturating_mul(space.f_frsize) / 5 * 4;
        ensure!(
            capacity >= 1024 * 1024 * 1024,
            "not enough free space for Firecracker XFS storage"
        );
        let staging = tempfile::tempdir_in(parent)?;
        let disk = staging.path().join("state.xfs");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&disk)?;
        file.set_len(capacity)?;
        checked("mkfs.xfs", &["-q", "-m", "reflink=1", path(&disk)?])?;
        let mount = staging.path().join("mount");
        fs::create_dir(&mount)?;
        mount_image(&disk, &mount)?;
        let copied = checked(
            "cp",
            &[
                "-a",
                "--sparse=always",
                "--",
                path(&root.join("."))?,
                path(&mount)?,
            ],
        );
        if let Err(error) = checked("umount", &[path(&mount)?]) {
            let retained = staging.keep();
            return Err(error).with_context(|| {
                format!(
                    "unmounting storage staging directory {}",
                    retained.display()
                )
            });
        }
        copied.context("preserving existing Firecracker state on XFS")?;
        file.sync_all()?;
        fs::rename(&disk, &image)?;
    }
    validate_trusted_file("Firecracker XFS volume", &image)?;
    mount_image(&image, &root)?;
    Ok(())
}

fn lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    flock(&file, FlockOperation::NonBlockingLockExclusive)
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

fn path(path: &Path) -> Result<&str> {
    path.to_str()
        .context("Firecracker storage path must be UTF-8")
}

fn mount_image(image: &Path, target: &Path) -> Result<()> {
    checked(
        "mount",
        &["-o", "loop,noatime", "--", path(image)?, path(target)?],
    )?;
    Ok(())
}

fn checked(program: &str, arguments: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new(trusted_host_command(program)?)
        .args(arguments)
        .output()?;
    ensure!(
        output.status.success(),
        "{program} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires root, ext4-backed /var/lib/exo, XFS tools, and loop mounts"]
    fn provisions_preserves_and_remounts_storage() -> Result<()> {
        let directory = tempfile::tempdir_in("/var/lib/exo")?;
        let config = FirecrackerConfig {
            state_root: directory.path().join("state.v1"),
            ..Default::default()
        };
        let root = &config.state_root;
        fs::create_dir(root)?;
        fs::write(root.join("original"), b"cached data")?;
        fs::hard_link(root.join("original"), root.join("link"))?;
        let backend_lock = lock(&root.join("backend.lock"))?;
        assert!(
            prepare(&config)
                .unwrap_err()
                .to_string()
                .contains("active Firecracker backend")
        );
        drop(backend_lock);
        let pid = jail_root(&config, "fc-test").join("firecracker.pid");
        fs::create_dir_all(pid.parent().unwrap())?;
        fs::write(&pid, std::process::id().to_string())?;
        assert!(
            prepare(&config)
                .unwrap_err()
                .to_string()
                .contains("stop running Firecracker")
        );
        fs::remove_file(pid)?;
        prepare(&config)?;
        let assertions = (|| -> Result<()> {
            assert_eq!(fs::read(root.join("original"))?, b"cached data");
            assert_eq!(
                fs::metadata(root.join("original"))?.ino(),
                fs::metadata(root.join("link"))?.ino()
            );
            checked(
                "cp",
                &[
                    "--reflink=always",
                    path(&root.join("original"))?,
                    path(&root.join("clone"))?,
                ],
            )?;
            fs::write(root.join("clone"), b"private change")?;
            assert_eq!(fs::read(root.join("original"))?, b"cached data");
            fs::write(root.join("original"), b"updated")?;
            prepare(&config)?;
            assert_eq!(fs::read(root.join("original"))?, b"updated");
            Ok(())
        })();
        if let Err(error) = checked("umount", &[path(root)?]) {
            let retained = directory.keep();
            return Err(error)
                .with_context(|| format!("test storage retained at {}", retained.display()));
        }
        assertions?;
        assert_eq!(fs::read(root.join("original"))?, b"cached data");
        prepare(&config)?;
        let persisted = fs::read(root.join("original"));
        if let Err(error) = checked("umount", &[path(root)?]) {
            let retained = directory.keep();
            return Err(error)
                .with_context(|| format!("test storage retained at {}", retained.display()));
        }
        assert_eq!(persisted?, b"updated");
        Ok(())
    }
}
