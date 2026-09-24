use super::*;
use crate::SandboxProvider;
use std::collections::BTreeMap;

impl ResourceStore {
    #[cfg(feature = "firecracker")]
    pub(crate) fn image_store(root: &Path, size_gib: u64) -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let mut store = Self::new(root)?;
            store.image_size_gib = Some(size_gib);
            store.initialize()?;
            let filesystem = checked(
                Command::new("findmnt")
                    .args(["-n", "-o", "FSTYPE", "-T"])
                    .arg(&store.root),
            )?;
            ensure!(
                String::from_utf8_lossy(&filesystem.stdout).trim() == "xfs",
                "Firecracker resources require an XFS state root with reflinks enabled: {}",
                root.display()
            );
            let probe = tempfile::tempdir_in(&store.root)?;
            fs::write(probe.path().join("source"), b"reflink probe")?;
            checked(
                Command::new("cp")
                    .arg("--reflink=always")
                    .arg(probe.path().join("source"))
                    .arg(probe.path().join("clone")),
            )
            .context("Firecracker resources require XFS with reflinks enabled")?;
            Ok(store)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (root, size_gib);
            bail!("resource images must be prepared on the Linux Firecracker host")
        }
    }

    #[cfg(feature = "firecracker")]
    pub(crate) fn materialize_images(
        &self,
        request: super::super::MaterializeResourcesRequest,
    ) -> Result<Vec<FileSystemMount>> {
        ensure!(
            !request.resume || self.has_thread(request.agent, request.thread),
            "saved thread resource images are missing from the Firecracker host"
        );
        for (snapshot, source) in request.archives {
            validate_key(&snapshot)?;
            let _lock = self.lock(&snapshot)?;
            if !self.root.join("snapshots").join(&snapshot).exists() {
                self.publish(&snapshot, |target| {
                    checked(
                        Command::new("tar")
                            .args(["--no-same-owner", "-xpf"])
                            .arg(&source)
                            .arg("-C")
                            .arg(target),
                    )?;
                    Ok(())
                })?;
            }
        }
        self.materialize(
            request.agent,
            request.thread,
            request.resources,
            request.credentials,
        )
    }

    pub(crate) fn external_provider(
        &self,
        agent: AgentId,
        thread: ThreadId,
    ) -> Result<Option<SandboxProvider>> {
        let path = self.thread_directory(agent, thread).join("provider.json");
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
    }

    pub(crate) fn remember_external(
        &self,
        agent: AgentId,
        thread: ThreadId,
        resources: Option<&[PreparedResource]>,
    ) -> Result<()> {
        let directory = self.thread_directory(agent, thread);
        fs::create_dir_all(&directory)?;
        fs::write(
            directory.join("provider.json"),
            serde_json::to_vec(&SandboxProvider::Firecracker)?,
        )?;
        if let Some(resources) = resources {
            fs::write(
                directory.join("materialized.json"),
                serde_json::to_vec(resources)?,
            )?;
        }
        Ok(())
    }

    pub(crate) fn with_image_sources<T>(
        &self,
        resources: &[PreparedResource],
        operation: impl FnOnce(BTreeMap<String, PathBuf>) -> Result<T>,
    ) -> Result<T> {
        self.initialize()?;
        let exports = self.root.join("exports");
        fs::create_dir_all(&exports)?;
        let mut sources = BTreeMap::new();
        for resource in resources {
            let Some(snapshot) = &resource.snapshot else {
                continue;
            };
            validate_key(snapshot)?;
            if sources.contains_key(snapshot) {
                continue;
            }
            let _lock = self.lock(&format!("export-{snapshot}"))?;
            let archive = exports.join(format!("{snapshot}.tar"));
            if !archive.exists() {
                let staging = tempfile::tempdir_in(&exports)?;
                let volume = staging.path().join("volume");
                fs::create_dir(&volume)?;
                clone_volume(&self.root.join("snapshots").join(snapshot), &volume)?;
                let workspace = mount_volume(&volume)?;
                let packed = checked(
                    Command::new("tar")
                        .env("COPYFILE_DISABLE", "1")
                        .arg("-cpf")
                        .arg(staging.path().join("resource.tar"))
                        .arg("-C")
                        .arg(workspace)
                        .arg("."),
                );
                if let Err(error) = unmount_volume(&volume) {
                    let path = staging.keep();
                    return Err(error).with_context(|| {
                        format!("unmounting resource export at {}", path.display())
                    });
                }
                packed?;
                fs::rename(staging.path().join("resource.tar"), &archive)?;
            }
            sources.insert(snapshot.clone(), archive);
        }
        operation(sources)
    }

    pub(super) fn create_volume(&self, directory: &Path) -> Result<()> {
        if let Some(size) = self.image_size_gib {
            let image = directory.join("volume.ext4");
            let file = File::create_new(&image)?;
            file.set_len(
                size.checked_mul(1024 * 1024 * 1024)
                    .context("resource image is too large")?,
            )?;
            checked(
                Command::new("mkfs.ext4")
                    .args([
                        "-q",
                        "-F",
                        "-m",
                        "0",
                        "-E",
                        "lazy_itable_init=1,lazy_journal_init=1",
                    ])
                    .arg(&image),
            )?;
            return Ok(());
        }
        create_volume(directory)
    }

    pub(super) fn mount_volume(&self, directory: &Path) -> Result<PathBuf> {
        if self.image_size_gib.is_some() {
            let mount = directory.join("mount");
            fs::create_dir_all(&mount)?;
            let status = Command::new("mountpoint").arg("-q").arg(&mount).status()?;
            if status.success() {
                return Ok(mount);
            }
            ensure!(
                status.code() == Some(32),
                "checking resource mount {} failed: {status}",
                mount.display()
            );
            checked(
                Command::new("mount")
                    .args(["-o", "loop,nosuid,nodev"])
                    .arg(directory.join("volume.ext4"))
                    .arg(&mount),
            )?;
            return Ok(mount);
        }
        mount_volume(directory)
    }

    pub(super) fn unmount_volume(&self, directory: &Path) -> Result<()> {
        if self.image_size_gib.is_some() {
            let mount = directory.join("mount");
            if mount.exists() {
                checked(Command::new("umount").arg(&mount))?;
                fs::remove_dir(mount)?;
            }
            return Ok(());
        }
        unmount_volume(directory)
    }

    pub(super) fn clone_volume(&self, source: &Path, target: &Path) -> Result<()> {
        if self.image_size_gib.is_some() {
            checked(
                Command::new("cp")
                    .args(["--reflink=always", "--"])
                    .arg(source.join("volume.ext4"))
                    .arg(target.join("volume.ext4")),
            )?;
            return Ok(());
        }
        clone_volume(source, target)
    }

    pub(super) fn prepare_ownership(&self, _workspace: &Path) -> Result<()> {
        #[cfg(target_os = "linux")]
        if self.image_size_gib.is_some() {
            own_resource(_workspace)?;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn own_resource(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, lchown};
    let metadata = fs::symlink_metadata(path)?;
    if metadata.uid() != 10001 || metadata.gid() != 10001 {
        lchown(path, Some(10001), Some(10001))?;
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            own_resource(&entry?.path())?;
        }
    }
    Ok(())
}

pub(crate) fn host_git_credential(url: &str) -> Result<Option<GitCredential>> {
    use std::io::Write;
    use std::process::Stdio;
    ensure!(
        !url.contains(['\n', '\r', '\0']),
        "invalid Git credential URL"
    );
    let mut child = Command::new("git")
        .args(["credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .context("Git credential input")?
        .write_all(format!("url={url}\n\n").as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout)?;
    let field = |name: &str| {
        value
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::to_owned)
    };
    match (field("username="), field("password=")) {
        (Some(username), Some(token)) => Ok(Some(GitCredential {
            identity: format!("host:{username}"),
            username,
            token,
        })),
        _ => bail!("Git credential helper returned incomplete credentials"),
    }
}
