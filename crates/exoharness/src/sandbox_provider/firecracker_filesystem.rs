use super::*;

fn validate_capture_runtime(version: &str) -> Result<()> {
    let version = semver::Version::parse(version.trim_start_matches('v'))?;
    ensure!(
        version >= semver::Version::new(1, 16, 2),
        "filesystem capture requires Firecracker 1.16.2 or newer: older versions hang vsock after bare pause/resume (upstream #6100)"
    );
    Ok(())
}

impl Shared {
    fn base_image_digest(&self, image: &Path) -> Result<String> {
        if let Some(digest) = self
            .base_image_digests
            .lock()
            .expect("image digest cache poisoned")
            .get(image)
            .cloned()
        {
            return Ok(digest);
        }
        let digest = super::super::firecracker_image::sha256_hex_of_file(image)?;
        self.base_image_digests
            .lock()
            .expect("image digest cache poisoned")
            .insert(image.to_owned(), digest.clone());
        Ok(digest)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FilesystemLayout {
    workdir: String,
    workspaces: Vec<crate::DurableFileSystem>,
    mounts: Vec<(String, crate::SandboxMountAccess)>,
}

impl FilesystemLayout {
    fn from_spec(spec: &SandboxSpec) -> Self {
        Self {
            workdir: spec.default_workdir.clone(),
            workspaces: spec.durable_file_systems.clone(),
            mounts: spec
                .mounts
                .iter()
                .map(|mount| (mount.guest_path.clone(), mount.access))
                .collect(),
        }
    }

    fn disk_names(&self) -> Vec<String> {
        let mut names = vec!["overlay.ext4".to_owned()];
        if !self.workspaces.is_empty() {
            names.push("workspace.ext4".to_owned());
        }
        names.extend((0..self.mounts.len()).map(|index| format!("resource-{index}.ext4")));
        names
    }

    fn sync_paths(&self) -> impl Iterator<Item = &str> {
        std::iter::once("/")
            .chain(
                self.workspaces
                    .iter()
                    .map(|workspace| workspace.mount_path.as_str()),
            )
            .chain(
                self.mounts
                    .iter()
                    .filter(|(_, access)| *access == crate::SandboxMountAccess::ReadWrite)
                    .map(|(path, _)| path.as_str()),
            )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FilesystemManifest {
    version: u32,
    key: String,
    sandbox_id: SandboxId,
    base_image_sha256: String,
    layout: FilesystemLayout,
    disk_sizes: Vec<u64>,
}

impl FilesystemManifest {
    fn from_payload(payload: &SnapshotPayload) -> Result<Self> {
        ensure!(
            payload.format == SnapshotFormat::FirecrackerFilesystemRef,
            "not a Firecracker filesystem snapshot"
        );
        let manifest: Self = serde_json::from_slice(&payload.bytes)?;
        ensure!(
            manifest.version == 1,
            "unsupported Firecracker filesystem snapshot version"
        );
        validate_snapshot_key(&manifest.key)?;
        validate_snapshot_key(&manifest.base_image_sha256)?;
        ensure!(
            manifest.disk_sizes.len() == manifest.layout.disk_names().len(),
            "filesystem snapshot disk set is incomplete"
        );
        ensure!(
            manifest.layout.workspaces.len() <= 1 && manifest.layout.mounts.len() <= 20,
            "invalid filesystem snapshot disk layout"
        );
        Ok(manifest)
    }

    fn payload(&self) -> Result<SnapshotPayload> {
        Ok(SnapshotPayload {
            format: SnapshotFormat::FirecrackerFilesystemRef,
            bytes: Bytes::from(serde_json::to_vec(self)?),
        })
    }

    fn directory(&self, config: &FirecrackerConfig) -> PathBuf {
        config
            .state_root
            .join("filesystem-snapshots")
            .join(stable_id(&self.sandbox_id))
            .join(&self.key)
    }

    fn staging_directory(&self, config: &FirecrackerConfig) -> Result<tempfile::TempDir> {
        let destination = self.directory(config);
        let parent = destination
            .parent()
            .context("missing filesystem snapshot parent")?;
        fs::create_dir_all(parent)?;
        Ok(tempfile::Builder::new()
            .prefix(".capture-")
            .tempdir_in(parent)?)
    }
}

/// Immutable disk files for checkpoint upload. Holding this guard prevents deletion.
pub struct FirecrackerFilesystemCapture {
    pub payload: SnapshotPayload,
    pub base_image: PathBuf,
    pub disks: Vec<PathBuf>,
    _lease: File,
}

fn open_capture(
    config: &FirecrackerConfig,
    payload: SnapshotPayload,
) -> Result<FirecrackerFilesystemCapture> {
    let manifest = FilesystemManifest::from_payload(&payload)?;
    let directory = manifest.directory(config);
    let lease = File::open(directory.join("lease"))
        .context("Firecracker filesystem snapshot is missing on this host")?;
    flock(&lease, FlockOperation::LockShared)?;
    let stored: FilesystemManifest =
        serde_json::from_reader(File::open(directory.join("manifest.json"))?.take(1_048_576))?;
    ensure!(
        stored.payload()?.bytes == payload.bytes,
        "filesystem snapshot manifest does not match its reference"
    );
    let disks: Vec<_> = manifest
        .layout
        .disk_names()
        .iter()
        .map(|name| directory.join(name))
        .collect();
    for (path, size) in disks.iter().zip(&manifest.disk_sizes) {
        let metadata =
            fs::symlink_metadata(path).context("required filesystem snapshot disk is missing")?;
        ensure!(
            metadata.is_file() && metadata.len() == *size && metadata.mode() & 0o222 == 0,
            "filesystem snapshot disk is invalid: {}",
            path.display()
        );
    }
    let base_image = directory.join("base.ext4");
    let base = fs::symlink_metadata(&base_image)?;
    ensure!(
        base.is_file() && base.mode() & 0o222 == 0,
        "filesystem snapshot base image is invalid"
    );
    Ok(FirecrackerFilesystemCapture {
        payload,
        base_image,
        disks,
        _lease: lease,
    })
}

fn allocated_bytes(directory: &Path) -> Result<u64> {
    snapshot_directory_bytes(directory, |file| {
        file.blocks()
            .checked_mul(512)
            .context("snapshot size overflow")
    })
}

fn publish_capture(
    config: &FirecrackerConfig,
    temporary: &Path,
    manifest: &FilesystemManifest,
) -> Result<()> {
    let budget_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config.state_root.join("filesystem-snapshot.lock"))?;
    budget_lock.lock()?;
    ensure!(
        allocated_bytes(&config.state_root.join("filesystem-snapshots"))? <= MAX_SNAPSHOT_BYTES,
        "Firecracker filesystem snapshot budget exhausted; delete unused snapshots"
    );
    fs::write(
        temporary.join("manifest.json"),
        serde_json::to_vec(manifest)?,
    )?;
    let mut names = manifest.layout.disk_names();
    names.extend(["base.ext4".to_owned(), "manifest.json".to_owned()]);
    let names: Vec<_> = names.iter().map(String::as_str).collect();
    seal_snapshot_files(temporary, &names)?;
    publish_snapshot_directory(temporary, &manifest.directory(config))?;
    Ok(())
}

fn capture_disks(
    config: &FirecrackerConfig,
    source: &MachineRecord,
    mut manifest: FilesystemManifest,
    suspend: bool,
) -> Result<SnapshotPayload> {
    let temporary = manifest.staging_directory(config)?;
    fs::hard_link(&source.resolved_image, temporary.path().join("base.ext4"))?;
    let root = jail_root(config, &source.machine_id);
    with_paused_snapshot_source(&root, &source.machine_id, suspend, || {
        for name in manifest.layout.disk_names() {
            copy_sparse_reflink(&root.join(&name), &temporary.path().join(&name))
                .with_context(|| format!("capturing required filesystem disk {name}"))?;
            manifest
                .disk_sizes
                .push(fs::metadata(temporary.path().join(name))?.len());
        }
        publish_capture(config, temporary.path(), &manifest)?;
        Ok(())
    })?;
    manifest.payload()
}

pub(super) fn delete_capture(config: &FirecrackerConfig, payload: SnapshotPayload) -> Result<()> {
    let manifest = FilesystemManifest::from_payload(&payload)?;
    let directory = manifest.directory(config);
    if !directory.try_exists()? {
        return Ok(());
    }
    let lease = File::open(directory.join("lease"))?;
    flock(&lease, FlockOperation::NonBlockingLockExclusive)
        .context("filesystem snapshot is still in use")?;
    fs::remove_dir_all(&directory)?;
    File::open(
        directory
            .parent()
            .context("missing filesystem snapshot parent")?,
    )?
    .sync_all()?;
    Ok(())
}

impl FirecrackerSandboxBackend {
    /// Opens the immutable files referenced by a filesystem snapshot for upload.
    pub async fn filesystem_snapshot_files(
        &self,
        payload: SnapshotPayload,
    ) -> Result<FirecrackerFilesystemCapture> {
        let config = self.shared.config.clone();
        tokio::task::spawn_blocking(move || open_capture(&config, payload)).await?
    }

    /// Imports verified, stable disk files, in capture order, onto this worker's XFS.
    pub async fn import_filesystem_snapshot(
        &self,
        payload: SnapshotPayload,
        base_image: PathBuf,
        disks: Vec<PathBuf>,
    ) -> Result<SnapshotPayload> {
        let config = self.shared.config.clone();
        tokio::task::spawn_blocking(move || {
            let mut manifest = FilesystemManifest::from_payload(&payload)?;
            ensure!(
                disks.len() == manifest.disk_sizes.len(),
                "filesystem import disk set is incomplete"
            );
            ensure!(
                super::super::firecracker_image::sha256_hex_of_file(&base_image)?
                    == manifest.base_image_sha256,
                "filesystem import base image does not match"
            );
            manifest.key = format!("{:x}", Sha256::digest(Uuid::new_v4().as_bytes()));
            let temporary = manifest.staging_directory(&config)?;
            copy_sparse_reflink(&base_image, &temporary.path().join("base.ext4"))?;
            for ((name, source), size) in manifest
                .layout
                .disk_names()
                .iter()
                .zip(&disks)
                .zip(&manifest.disk_sizes)
            {
                ensure!(
                    fs::metadata(source)?.len() == *size,
                    "filesystem import disk size does not match: {name}"
                );
                copy_sparse_reflink(source, &temporary.path().join(name))?;
            }
            publish_capture(&config, temporary.path(), &manifest)?;
            manifest.payload()
        })
        .await?
    }

    pub(super) async fn acquire_filesystem_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let terminate = self.terminate(request.clone());
        self.egress
            .restore(
                request.clone(),
                |policy| async move { self.egress_transport(&policy).await },
                |egress| {
                    let backend = self.clone();
                    async move {
                        tokio::spawn(async move {
                            backend
                                .acquire_filesystem_request(
                                    FirecrackerRequest {
                                        sandbox: request,
                                        egress_proxy: egress
                                            .as_ref()
                                            .map(|egress| egress.endpoints()),
                                    },
                                    payload,
                                    egress,
                                )
                                .await
                        })
                        .await?
                    }
                },
                terminate,
            )
            .await
            .map(|handle| crate::with_process_management(handle))
    }

    pub(in crate::sandbox_provider) async fn acquire_filesystem_request(
        &self,
        request: FirecrackerRequest,
        payload: SnapshotPayload,
        egress: Option<Arc<SandboxEgress>>,
    ) -> Result<FirecrackerSandboxHandle> {
        let manifest = FilesystemManifest::from_payload(&payload)?;
        ensure!(
            request.lifecycle.idle_ttl.is_some(),
            "filesystem restore requires a warm sandbox"
        );
        ensure!(
            FilesystemLayout::from_spec(&request.spec) == manifest.layout,
            "filesystem restore mount layout does not match"
        );
        let capture = self.filesystem_snapshot_files(payload).await?;
        let resolved = self.resolve_request(request).await?;
        let spec_hash = sandbox_spec_hash(&resolved.spec);
        let id = machine_id(&resolved.sandbox_id, &spec_hash);
        let _guard = self
            .shared
            .lifecycle_locks
            .lock_sandbox(&resolved.sandbox_id)
            .await;
        let config = self.shared.config.clone();
        let request_for_validation = resolved.clone();
        let shared = Arc::clone(&self.shared);
        tokio::task::spawn_blocking(move || {
            let prefix = format!("fc-{}-", stable_id(&request_for_validation.sandbox_id));
            for entry in fs::read_dir(config.state_root.join("manifests"))? {
                ensure!(
                    !entry?.file_name().to_string_lossy().starts_with(&prefix),
                    "filesystem restore target already has an allocation"
                );
            }
            ensure!(
                shared.base_image_digest(Path::new(&request_for_validation.spec.image))?
                    == manifest.base_image_sha256,
                "filesystem restore base image does not match"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        let reservation = self.shared.reserve_machine_capacity(&id).await?;
        let record = self
            .shared
            .new_machine_record(&resolved, &id, &spec_hash, None)
            .await?;
        let config = self.shared.config.clone();
        let request_for_disks = resolved.clone();
        let result = async {
            tokio::task::spawn_blocking(move || {
                install_disks(&config, &record, &request_for_disks, &capture)
            })
            .await??;
            let mut handle = self
                .acquire_resolved_locked(resolved, spec_hash, id.clone(), false, Some(reservation))
                .await?;
            if let Some(egress) = egress {
                self.track_egress(&handle, egress.transport()).await?;
                egress
                    .initialize(&handle, handle.machine.record.network().guest_ip)
                    .await?;
                handle.egress = Some(egress);
            }
            Ok(handle)
        }
        .await;
        if let Err(error) = result {
            self.shared
                .warm_machines
                .lock()
                .await
                .retain(|_, entry| entry.machine_id != id);
            return match self.shared.cleanup_machine(&id, true).await {
                Ok(()) => Err(error),
                Err(cleanup) => {
                    Err(error.context(format!("filesystem restore cleanup failed: {cleanup:#}")))
                }
            };
        }
        result
    }
}

pub(super) async fn snapshot_filesystem(
    shared: Arc<Shared>,
    mut request: SandboxRequest,
    suspend: bool,
) -> Result<SnapshotPayload> {
    tokio::spawn(async move {
        let guard = shared
            .lifecycle_locks
            .lock_sandbox(&request.sandbox_id)
            .await;
        validate_capture_runtime(&shared.host_fingerprint.firecracker_version)?;
        ensure!(
            request.lifecycle.idle_ttl.is_some(),
            "filesystem capture requires a warm sandbox"
        );
        let entry = shared
            .warm_machines
            .lock()
            .await
            .get(&request.sandbox_id)
            .cloned();
        let machine_id = match entry {
            Some(entry) => entry.machine_id,
            None => {
                request
                    .provider_state
                    .as_ref()
                    .map(parse_provider_state)
                    .transpose()?
                    .context("filesystem capture requires the existing sandbox's provider state")?
                    .machine_id
            }
        };
        ensure!(
            valid_machine_id(&machine_id)
                && machine_id.starts_with(&format!("fc-{}-", stable_id(&request.sandbox_id))),
            "filesystem capture provider state belongs to another sandbox"
        );
        let source = shared
            .load_machine_record(&machine_id)
            .await?
            .context("filesystem capture source is missing")?;
        request.spec.image = source.resolved_image.clone();
        ensure!(
            sandbox_spec_hash(&request.spec) == source.spec_hash,
            "filesystem capture specification changed"
        );
        let layout = FilesystemLayout::from_spec(&request.spec);
        if process_running(&shared.pid_path(&machine_id)) {
            let guest = GuestClient::new(
                Arc::clone(&shared),
                machine_from_record(&shared.config, source.clone()).vsock_path,
            );
            for path in layout.sync_paths() {
                guest
                    .sync_filesystem(&path)
                    .await
                    .context("syncing filesystem capture source")?;
            }
        }
        let capture_shared = Arc::clone(&shared);
        let source_for_capture = source.clone();
        let (payload, _guard) = tokio::task::spawn_blocking(move || {
            let manifest = FilesystemManifest {
                version: 1,
                key: format!("{:x}", Sha256::digest(Uuid::new_v4().as_bytes())),
                sandbox_id: request.sandbox_id,
                base_image_sha256: capture_shared
                    .base_image_digest(Path::new(&source_for_capture.resolved_image))?,
                layout,
                disk_sizes: Vec::new(),
            };
            let payload = capture_disks(
                &capture_shared.config,
                &source_for_capture,
                manifest,
                suspend,
            )?;
            Ok::<_, anyhow::Error>((payload, guard))
        })
        .await??;
        if suspend {
            shared
                .warm_machines
                .lock()
                .await
                .retain(|_, entry| entry.machine_id != source.machine_id);
            shared.cleanup_machine(&source.machine_id, true).await?;
        }
        Ok(payload)
    })
    .await?
}

fn install_disks(
    config: &FirecrackerConfig,
    record: &MachineRecord,
    request: &FirecrackerRequest,
    capture: &FirecrackerFilesystemCapture,
) -> Result<()> {
    let root = jail_root(config, &record.machine_id);
    fs::create_dir_all(&root)?;
    let mut targets = vec![root.join("overlay.ext4")];
    if let Some(workspace) = &record.workspace_id {
        targets.push(
            config
                .state_root
                .join("workspaces")
                .join(format!("{workspace}.ext4")),
        );
    }
    let mut resource_targets = Vec::new();
    for mount in &request.spec.mounts {
        let target = resource_disk(config, request.scope, &mount.host_path)?;
        resource_targets.push(target.clone());
        targets.push(target);
    }
    resource_targets.sort();
    resource_targets.dedup();
    let mut locks = Vec::new();
    for target in resource_targets {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(target.with_extension("lock"))?;
        lock.lock()?;
        locks.push(lock);
    }
    for target in &targets {
        if target.try_exists()? {
            ensure!(
                fs::symlink_metadata(target)?.is_file() && fs::metadata(target)?.nlink() == 1,
                "filesystem restore disk is already attached: {}",
                target.display()
            );
        }
    }
    for (source, target) in capture.disks.iter().zip(targets) {
        let parent = target.parent().context("missing restored disk parent")?;
        let temporary = tempfile::NamedTempFile::new_in(parent)?;
        copy_sparse_reflink(source, temporary.path())?;
        fs::set_permissions(temporary.path(), Permissions::from_mode(0o600))?;
        temporary.as_file().sync_all()?;
        temporary.persist(&target)?;
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "firecracker_filesystem_tests.rs"]
mod tests;
