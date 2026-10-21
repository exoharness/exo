use super::*;
use futures::{StreamExt, TryStreamExt};

const MAX_CAPTURE_MOUNTS: usize = 20;

/// An externally served disk with a regular backing file for filesystem captures.
pub trait FirecrackerFilesystemDisk: Send + Sync {
    fn backing_file(&self) -> &Path;
    fn device_path(&self) -> &Path;
    /// Capture while the guest is paused. Return whether every range is materialized.
    fn capture(&self, destination: &Path) -> Result<bool>;
}

#[derive(Clone)]
pub(super) struct AttachedDisks {
    key: String,
    disks: Vec<Arc<dyn FirecrackerFilesystemDisk>>,
}

fn validate_attached_disks(
    manifest: &FilesystemManifest,
    attached: Option<&AttachedDisks>,
) -> Result<()> {
    ensure!(
        !manifest.partial || attached.is_some(),
        "partial filesystem snapshot requires external disks"
    );
    if let Some(attached) = attached {
        ensure!(
            attached.key == manifest.key,
            "external disks belong to another filesystem snapshot"
        );
    }
    Ok(())
}

pub struct FirecrackerFilesystemDisks {
    shared: Arc<Shared>,
    sandbox_id: SandboxId,
    key: String,
}

impl Drop for FirecrackerFilesystemDisks {
    fn drop(&mut self) {
        let mut disks = self
            .shared
            .filesystem_disks
            .lock()
            .expect("filesystem disks poisoned");
        if disks
            .get(&self.sandbox_id)
            .is_some_and(|entry| entry.key == self.key)
        {
            disks.remove(&self.sandbox_id);
        }
    }
}

fn new_capture_key() -> String {
    format!("{:x}", Sha256::digest(Uuid::new_v4().as_bytes()))
}

fn validate_capture_runtime(version: &str) -> Result<()> {
    let version = semver::Version::parse(version.trim_start_matches('v'))?;
    ensure!(
        version >= semver::Version::new(1, 16, 2),
        "filesystem capture requires Firecracker 1.16.2 or newer: older versions hang vsock after bare pause/resume (upstream #6100)"
    );
    Ok(())
}

fn base_image_identity(image: &Path) -> Result<ImageIdentity> {
    let metadata = fs::metadata(image)?;
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    ))
}

impl Shared {
    fn base_image_digest(&self, image: &Path) -> Result<String> {
        let identity = base_image_identity(image)?;
        if let Some(digest) = self
            .base_image_digests
            .lock()
            .expect("image digest cache poisoned")
            .get(&identity)
            .cloned()
        {
            return Ok(digest);
        }
        let digest = super::super::firecracker_image::sha256_hex_of_file(image)?;
        self.base_image_digests
            .lock()
            .expect("image digest cache poisoned")
            .insert(identity, digest.clone());
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FilesystemManifest {
    #[serde(default)]
    partial: bool,
    template: bool,
    key: String,
    sandbox_id: SandboxId,
    base_image_sha256: String,
    layout: FilesystemLayout,
    disk_sizes: Vec<u64>,
}

impl FilesystemManifest {
    fn from_payload(payload: &SnapshotPayload) -> Result<Self> {
        ensure!(
            payload.format == SnapshotFormat::FirecrackerFilesystemRef
                || payload.format == SnapshotFormat::FirecrackerLazyFilesystemRef,
            "not a Firecracker filesystem snapshot"
        );
        serde_json::from_slice::<Self>(&payload.bytes)?.validate()
    }

    fn validate(self) -> Result<Self> {
        validate_snapshot_key(&self.key)?;
        validate_snapshot_key(&self.base_image_sha256)?;
        ensure!(
            self.disk_sizes.len() == self.layout.disk_names().len(),
            "filesystem snapshot disk set is incomplete"
        );
        ensure!(
            self.layout.workspaces.len() <= 1 && self.layout.mounts.len() <= MAX_CAPTURE_MOUNTS,
            "invalid filesystem snapshot disk layout"
        );
        Ok(self)
    }

    fn payload(&self) -> Result<SnapshotPayload> {
        Ok(SnapshotPayload {
            format: if self.partial {
                SnapshotFormat::FirecrackerLazyFilesystemRef
            } else {
                SnapshotFormat::FirecrackerFilesystemRef
            },
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

    fn staging_directory(
        &self,
        config: &FirecrackerConfig,
    ) -> Result<(tempfile::TempDir, SnapshotLease)> {
        let destination = self.directory(config);
        capture_store::staging_directory(
            destination
                .parent()
                .context("missing filesystem snapshot parent")?,
            &config.state_root.join("snapshot-budget.lock"),
        )
    }
}

/// Immutable disk files for checkpoint upload. Holding this guard prevents deletion.
pub struct FirecrackerFilesystemCapture {
    pub complete: bool,
    pub base_image: PathBuf,
    pub disks: Vec<PathBuf>,
    _lease: SnapshotLease,
}

fn open_capture(
    config: &FirecrackerConfig,
    payload: SnapshotPayload,
) -> Result<FirecrackerFilesystemCapture> {
    let manifest = FilesystemManifest::from_payload(&payload)?;
    let directory = manifest.directory(config);
    let lease = capture_store::read_lease(&directory)?;
    let stored: FilesystemManifest = capture_store::read_manifest(&directory)?;
    ensure!(
        stored == manifest,
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
        complete: !manifest.partial,
        base_image,
        disks,
        _lease: lease,
    })
}

fn publish_capture(
    config: &FirecrackerConfig,
    temporary: &Path,
    manifest: &FilesystemManifest,
) -> Result<()> {
    fs::write(
        temporary.join("manifest.json"),
        serde_json::to_vec(manifest)?,
    )?;
    let mut names = manifest.layout.disk_names();
    names.extend(["base.ext4".to_owned(), "manifest.json".to_owned()]);
    let names: Vec<_> = names.iter().map(String::as_str).collect();
    seal_snapshot_files(temporary, &names)?;
    capture_store::publish(
        temporary,
        &manifest.directory(config),
        &config.state_root.join("snapshot-budget.lock"),
        || enforce_snapshot_budget(config),
    )
}

fn capture_disks(
    config: &FirecrackerConfig,
    source: &MachineRecord,
    mut manifest: FilesystemManifest,
    suspend: bool,
    attached: Option<&AttachedDisks>,
) -> Result<SnapshotPayload> {
    let (temporary, staging_lease) = manifest.staging_directory(config)?;
    let root = jail_root(config, &source.machine_id);
    fs::hard_link(root.join("rootfs.ext4"), temporary.path().join("base.ext4"))?;
    let captured = with_paused_snapshot_source(&root, &source.machine_id, suspend, || {
        for (index, name) in manifest.layout.disk_names().iter().enumerate() {
            let destination = temporary.path().join(name);
            if let Some(attached) = attached {
                manifest.partial |= !attached.disks[index].capture(&destination)?;
            } else {
                copy_sparse_reflink(&root.join(name), &destination)
                    .with_context(|| format!("capturing required filesystem disk {name}"))?;
            }
            manifest
                .disk_sizes
                .push(fs::metadata(temporary.path().join(name))?.len());
        }
        if suspend {
            publish_capture(config, temporary.path(), &manifest)?;
        }
        Ok(())
    });
    if let Err(error) = captured {
        drop(staging_lease);
        let directory = manifest.directory(config);
        if let Err(cleanup) = capture_store::delete(&directory) {
            return Err(error.context(format!("deleting failed filesystem capture: {cleanup:#}")));
        }
        return Err(error);
    }
    if !suspend {
        publish_capture(config, temporary.path(), &manifest)?;
    }
    manifest.payload()
}

pub(super) fn delete_capture(config: &FirecrackerConfig, payload: SnapshotPayload) -> Result<()> {
    capture_store::delete(&FilesystemManifest::from_payload(&payload)?.directory(config))
}

impl FirecrackerSandboxBackend {
    /// Pin an immutable cached base into the caller's private staging directory.
    pub async fn link_cached_filesystem_base(
        &self,
        payload: SnapshotPayload,
        destination: PathBuf,
    ) -> Result<bool> {
        let manifest = FilesystemManifest::from_payload(&payload)?;
        let config = self.shared.config.clone();
        tokio::task::spawn_blocking(move || {
            let _budget = snapshot_budget_lock(&config)?;
            let cached = base_image_cache_path(&config, &manifest.base_image_sha256);
            match fs::symlink_metadata(&cached) {
                Ok(metadata) => ensure!(
                    metadata.is_file() && metadata.mode() & 0o222 == 0,
                    "cached filesystem base is invalid"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            }
            fs::hard_link(cached, &destination)?;
            File::open(
                destination
                    .parent()
                    .context("missing base staging directory")?,
            )?
            .sync_all()?;
            Ok(true)
        })
        .await?
    }

    pub fn register_filesystem_disks(
        &self,
        sandbox_id: SandboxId,
        payload: SnapshotPayload,
        disks: Vec<Arc<dyn FirecrackerFilesystemDisk>>,
    ) -> Result<FirecrackerFilesystemDisks> {
        let manifest = FilesystemManifest::from_payload(&payload)?;
        ensure!(
            manifest.sandbox_id == sandbox_id && !manifest.template,
            "external filesystem disks belong to another sandbox"
        );
        ensure!(
            disks.len() == manifest.disk_sizes.len(),
            "external filesystem disk set is incomplete"
        );
        for (disk, size) in disks.iter().zip(&manifest.disk_sizes) {
            ensure!(
                fs::metadata(disk.backing_file())?.is_file()
                    && fs::metadata(disk.backing_file())?.len() == *size,
                "external filesystem backing size mismatch"
            );
        }
        let mut attached = self
            .shared
            .filesystem_disks
            .lock()
            .expect("filesystem disks poisoned");
        ensure!(
            !attached.contains_key(&sandbox_id),
            "sandbox already has external filesystem disks"
        );
        attached.insert(
            sandbox_id.clone(),
            AttachedDisks {
                key: manifest.key.clone(),
                disks,
            },
        );
        Ok(FirecrackerFilesystemDisks {
            shared: Arc::clone(&self.shared),
            sandbox_id,
            key: manifest.key,
        })
    }

    /// Import a partial disk set that can only boot with registered external disks.
    pub async fn import_lazy_filesystem_snapshot(
        &self,
        payload: SnapshotPayload,
        base_image: PathBuf,
        disks: Vec<PathBuf>,
    ) -> Result<SnapshotPayload> {
        self.import_filesystem_snapshot_inner(payload, base_image, disks, false, true)
            .await
    }

    /// Opens the immutable files referenced by a filesystem snapshot for upload.
    /// Used by the hosted sandbox service for durable checkpoint uploads.
    pub async fn filesystem_snapshot_files(
        &self,
        payload: SnapshotPayload,
    ) -> Result<FirecrackerFilesystemCapture> {
        let config = self.shared.config.clone();
        tokio::task::spawn_blocking(move || open_capture(&config, payload)).await?
    }

    /// Imports verified, stable disk files, in capture order, onto this worker's XFS.
    /// Used by the hosted sandbox service for durable checkpoint recovery.
    pub async fn import_filesystem_snapshot(
        &self,
        payload: SnapshotPayload,
        base_image: PathBuf,
        disks: Vec<PathBuf>,
    ) -> Result<SnapshotPayload> {
        self.import_filesystem_snapshot_inner(payload, base_image, disks, true, false)
            .await
    }

    /// Imports files whose contents and identity have already been verified by the caller.
    /// The caller must authenticate the snapshot metadata and verify every disk against it.
    /// Files must remain stable until import completes, as with ordinary filesystem import.
    pub async fn import_verified_filesystem_snapshot(
        &self,
        payload: SnapshotPayload,
        base_image: PathBuf,
        disks: Vec<PathBuf>,
    ) -> Result<SnapshotPayload> {
        self.import_filesystem_snapshot_inner(payload, base_image, disks, false, false)
            .await
    }

    async fn import_filesystem_snapshot_inner(
        &self,
        payload: SnapshotPayload,
        base_image: PathBuf,
        disks: Vec<PathBuf>,
        verify_base_digest: bool,
        partial: bool,
    ) -> Result<SnapshotPayload> {
        let config = self.shared.config.clone();
        tokio::task::spawn_blocking(move || {
            let mut manifest = FilesystemManifest::from_payload(&payload)?;
            ensure!(
                disks.len() == manifest.disk_sizes.len(),
                "filesystem import disk set is incomplete"
            );
            manifest.key = new_capture_key();
            manifest.partial = partial || (verify_base_digest && manifest.partial);
            let (temporary, _staging_lease) = manifest.staging_directory(&config)?;
            prepare_imported_base(
                &config,
                &base_image,
                &manifest.base_image_sha256,
                &temporary.path().join("base.ext4"),
                verify_base_digest,
            )?;
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

    /// Used by the hosted sandbox service for local checkpoint garbage collection.
    pub async fn collect_filesystem_snapshots(&self, keep: Vec<SnapshotPayload>) -> Result<usize> {
        let config = self.shared.config.clone();
        tokio::task::spawn_blocking(move || collect_captures(&config, keep)).await?
    }

    pub(super) async fn acquire_filesystem_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let terminate = self.shutdown_request(request.clone(), ShutdownMode::Terminate);
        self.egress
            .acquire(
                crate::egress::AcquisitionMode::Restore,
                request.clone(),
                self.external_proxy.as_ref(),
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
        let attached = self
            .shared
            .filesystem_disks
            .lock()
            .expect("filesystem disks poisoned")
            .get(&request.sandbox_id)
            .cloned();
        validate_attached_disks(&manifest, attached.as_ref())?;
        let template = manifest.template;
        ensure!(
            (template
                && manifest.layout.workdir == request.spec.default_workdir
                && manifest.layout.mounts.is_empty()
                && manifest.layout.workspaces.is_empty())
                || (!template && FilesystemLayout::from_spec(&request.spec) == manifest.layout),
            "filesystem restore mount layout does not match"
        );
        let capture = Arc::new(self.filesystem_snapshot_files(payload).await?);
        let mut resolved = prepare_request(request)?;
        validate_resource_shape(resolved.spec.resources.unwrap_or_default())?;
        let config = self.shared.config.clone();
        let capture_for_image = Arc::clone(&capture);
        let digest = manifest.base_image_sha256.clone();
        let shared = Arc::clone(&self.shared);
        let image = tokio::task::spawn_blocking(move || {
            let image = cache_base_image(&config, &capture_for_image.base_image, &digest)?;
            shared
                .base_image_digests
                .lock()
                .expect("image digest cache poisoned")
                .insert(base_image_identity(&image)?, digest);
            Ok::<_, anyhow::Error>(image)
        })
        .await??;
        resolved.sandbox.spec.image = image
            .to_str()
            .context("cached base path is not UTF-8")?
            .to_owned();
        let spec_hash = sandbox_spec_hash(&resolved.spec);
        let id = machine_id(&resolved.sandbox_id, &spec_hash);
        let mut lifecycle_guard = Some(
            self.shared
                .lifecycle_locks
                .lock_sandbox(&resolved.sandbox_id)
                .await,
        );
        let config = self.shared.config.clone();
        let request_for_validation = resolved.clone();
        tokio::task::spawn_blocking(move || {
            let prefix = format!("fc-{}-", stable_id(&request_for_validation.sandbox_id));
            for entry in fs::read_dir(config.state_root.join("manifests"))? {
                ensure!(
                    !entry?.file_name().to_string_lossy().starts_with(&prefix),
                    "filesystem restore target already has an allocation"
                );
            }
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
        let capture_for_disks = Arc::clone(&capture);
        let result = async {
            let disks = tokio::task::spawn_blocking(move || {
                install_disks(
                    &config,
                    &record,
                    &request_for_disks,
                    &capture_for_disks,
                    template,
                    attached.as_ref(),
                )
            })
            .await??;
            let mut handle = self
                .acquire_resolved_locked(
                    resolved,
                    spec_hash,
                    id.clone(),
                    false,
                    Some(reservation),
                    !template,
                )
                .await?;
            if let Some(egress) = egress {
                drop(lifecycle_guard.take());
                self.track_egress(&handle, egress.transport()).await?;
                egress
                    .initialize(&handle, handle.machine.record.network().guest_ip)
                    .await?;
                handle.egress = Some(egress);
            }
            tokio::task::spawn_blocking(move || disks.commit()).await??;
            Ok(handle)
        }
        .await;
        match result {
            Ok(handle) => Ok(handle),
            Err(error) => {
                let _guard = match lifecycle_guard {
                    Some(guard) => guard,
                    None => self.shared.lifecycle_locks.lock_machine(&id).await,
                };
                self.shared
                    .warm_machines
                    .lock()
                    .await
                    .retain(|_, entry| entry.machine_id != id);
                match self.shared.cleanup_machine(&id, true).await {
                    Ok(()) => Err(error),
                    Err(cleanup) => {
                        Err(error
                            .context(format!("filesystem restore cleanup failed: {cleanup:#}")))
                    }
                }
            }
        }
    }
}

pub(super) async fn snapshot_filesystem(
    shared: Arc<Shared>,
    mut request: SandboxRequest,
    suspend: bool,
    template: bool,
) -> Result<SnapshotPayload> {
    tokio::spawn(async move {
        let _guard = shared
            .lifecycle_locks
            .lock_sandbox(&request.sandbox_id)
            .await;
        validate_capture_runtime(&shared.host_fingerprint.firecracker_version)?;
        if template {
            ensure!(
                request.spec.policy == SandboxNetworkPolicy::Disabled.into()
                    && request.spec.mounts.is_empty()
                    && request.spec.durable_file_systems.is_empty(),
                "filesystem templates require disabled networking and no mounted resources"
            );
        }
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
            futures::stream::iter(
                layout
                    .sync_paths()
                    .map(|path| guest.sync_filesystem(path))
                    .collect::<Vec<_>>(),
            )
            .buffer_unordered(8)
            .try_collect::<Vec<_>>()
            .await
            .context("syncing filesystem capture source")?;
        }
        let attached = shared.filesystem_disks.lock().expect("filesystem disks poisoned")
            .get(&request.sandbox_id).cloned();
        let capture_shared = Arc::clone(&shared);
        let source_for_capture = source.clone();
        let payload = tokio::task::spawn_blocking(move || {
            let manifest = FilesystemManifest {
                partial: false,
                template,
                key: new_capture_key(),
                sandbox_id: request.sandbox_id,
                base_image_sha256: capture_shared.base_image_digest(
                    &jail_root(&capture_shared.config, &source_for_capture.machine_id)
                        .join("rootfs.ext4"),
                )?,
                layout,
                disk_sizes: Vec::new(),
            };
            capture_disks(
                &capture_shared.config,
                &source_for_capture,
                manifest,
                suspend,
                attached.as_ref(),
            )
        })
        .await??;
        if suspend {
            shared
                .warm_machines
                .lock()
                .await
                .retain(|_, entry| entry.machine_id != source.machine_id);
            if let Err(error) = shared.cleanup_machine(&source.machine_id, true).await {
                tracing::warn!(machine_id = %source.machine_id, %error, "cleanup after filesystem suspension failed");
            }
        }
        Ok(payload)
    })
    .await?
}

pub(super) fn shared_base_image_inodes(config: &FirecrackerConfig) -> Result<HashSet<(u64, u64)>> {
    let root = config.state_root.join("filesystem-snapshots");
    let mut bases = HashMap::new();
    for sandbox in capture_store::directories(&root)? {
        for capture in capture_store::directories(&sandbox?)? {
            let metadata = match fs::symlink_metadata(capture?.join("base.ext4")) {
                Ok(metadata) if metadata.is_file() => metadata,
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let entry = bases
                .entry((metadata.dev(), metadata.ino()))
                .or_insert((metadata.nlink(), 0u64));
            entry.1 += 1;
        }
    }
    Ok(bases
        .into_iter()
        .filter_map(|(inode, (links, captures))| (links > captures).then_some(inode))
        .collect())
}

fn collect_captures(config: &FirecrackerConfig, keep: Vec<SnapshotPayload>) -> Result<usize> {
    let keep = keep
        .iter()
        .map(|payload| Ok(FilesystemManifest::from_payload(payload)?.directory(config)))
        .collect::<Result<_>>()?;
    let root = config.state_root.join("filesystem-snapshots");
    if !root.try_exists()? {
        return Ok(0);
    }
    let _budget = snapshot_budget_lock(config)?;
    let mut removed = 0;
    for sandbox in capture_store::directories(&root)? {
        removed += capture_store::collect(&sandbox?, &keep, |directory| {
            let manifest: FilesystemManifest = capture_store::read_manifest(directory)?;
            Ok(manifest.validate()?.directory(config))
        })?;
    }
    Ok(removed)
}

fn base_image_cache_path(config: &FirecrackerConfig, digest: &str) -> PathBuf {
    config
        .state_root
        .join("images/filesystem")
        .join(format!("{digest}.ext4"))
}

fn prepare_imported_base(
    config: &FirecrackerConfig,
    source: &Path,
    digest: &str,
    target: &Path,
    verify_digest: bool,
) -> Result<()> {
    let cached = base_image_cache_path(config, digest);
    if !cached.try_exists()? {
        copy_sparse_reflink(source, target)?;
        if verify_digest {
            ensure!(
                super::super::firecracker_image::sha256_hex_of_file(target)? == digest,
                "filesystem import base image does not match"
            );
        }
        fs::set_permissions(target, Permissions::from_mode(0o444))?;
        cache_base_image(config, target, digest)?;
        fs::remove_file(target)?;
    }
    fs::hard_link(cached, target)?;
    Ok(())
}

fn cache_base_image(config: &FirecrackerConfig, source: &Path, digest: &str) -> Result<PathBuf> {
    let cached = base_image_cache_path(config, digest);
    let parent = cached.parent().context("missing image cache parent")?;
    fs::create_dir_all(parent)?;
    match fs::hard_link(source, &cached) {
        Ok(()) => {
            File::open(&cached)?.sync_all()?;
            File::open(parent)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("pinning filesystem capture base image"),
    }
    Ok(cached.canonicalize()?)
}

struct InstalledDisks {
    targets: Vec<(PathBuf, PathBuf)>,
    _locks: Vec<SnapshotLease>,
}

impl InstalledDisks {
    fn commit(self) -> Result<()> {
        struct Replacement {
            target: PathBuf,
            next: tempfile::NamedTempFile,
            previous: Option<tempfile::NamedTempFile>,
        }
        fn staged_link(source: &Path, target: &Path) -> Result<tempfile::NamedTempFile> {
            let temporary = tempfile::NamedTempFile::new_in(
                target.parent().context("restored disk has no parent")?,
            )?;
            fs::remove_file(temporary.path())?;
            fs::hard_link(source, temporary.path())?;
            Ok(temporary)
        }
        let mut replacements = Vec::new();
        for (source, target) in &self.targets {
            let previous = if target.try_exists()? {
                Some(staged_link(target, target)?)
            } else {
                None
            };
            replacements.push(Replacement {
                target: target.clone(),
                next: staged_link(source, target)?,
                previous,
            });
        }
        let mut swapped = 0;
        let result = (|| {
            for replacement in &replacements {
                fs::rename(replacement.next.path(), &replacement.target)?;
                swapped += 1;
                File::open(
                    replacement
                        .target
                        .parent()
                        .context("restored disk has no parent")?,
                )?
                .sync_all()?;
            }
            Ok::<_, anyhow::Error>(())
        })();
        if result.is_err() {
            for replacement in replacements.iter_mut().take(swapped).rev() {
                let restored = match &replacement.previous {
                    Some(previous) => fs::rename(previous.path(), &replacement.target),
                    None => fs::remove_file(&replacement.target),
                };
                if let Err(error) = restored {
                    if let Some(previous) = replacement.previous.take() {
                        match previous.keep() {
                            Ok((_, backup)) => {
                                tracing::error!(path = %replacement.target.display(), backup = %backup.display(), %error, "failed to roll back restored disk")
                            }
                            Err(keep) => {
                                tracing::error!(%keep, %error, "failed to preserve restored disk backup")
                            }
                        }
                    } else {
                        tracing::error!(path = %replacement.target.display(), %error, "failed to roll back restored disk");
                    }
                }
            }
        }
        result
    }
}

fn lock_restore_destinations(targets: &[(PathBuf, Option<PathBuf>)]) -> Result<Vec<SnapshotLease>> {
    let mut persistent = targets
        .iter()
        .filter_map(|(_, target)| target.clone())
        .collect::<Vec<_>>();
    persistent.sort();
    ensure!(
        persistent.windows(2).all(|pair| pair[0] != pair[1]),
        "duplicate filesystem restore destination"
    );
    let mut locks = Vec::new();
    for target in persistent {
        locks.push(capture_store::exclusive_lock(
            &target.with_extension("lock"),
        )?);
        if target.try_exists()? {
            ensure!(
                fs::symlink_metadata(&target)?.is_file() && fs::metadata(&target)?.nlink() == 1,
                "filesystem restore disk is already attached: {}",
                target.display()
            );
        }
    }
    Ok(locks)
}

fn install_disks(
    config: &FirecrackerConfig,
    record: &MachineRecord,
    request: &FirecrackerRequest,
    capture: &FirecrackerFilesystemCapture,
    template: bool,
    attached: Option<&AttachedDisks>,
) -> Result<InstalledDisks> {
    let root = jail_root(config, &record.machine_id);
    fs::create_dir_all(&root)?;
    let mut targets = vec![(root.join("overlay.ext4"), None)];
    if !template {
        if let Some(workspace) = &record.workspace_id {
            targets.push((
                root.join("workspace.ext4"),
                Some(
                    config
                        .state_root
                        .join("workspaces")
                        .join(format!("{workspace}.ext4")),
                ),
            ));
        }
        for (index, mount) in request.spec.mounts.iter().enumerate() {
            targets.push((
                root.join(format!("resource-{index}.ext4")),
                Some(resource_disk(config, request.scope, &mount.host_path)?),
            ));
        }
    }
    let locks = lock_restore_destinations(&targets)?;
    ensure!(
        capture.disks.len() == targets.len(),
        "capture disk count differs from restore layout"
    );
    let mut installed = InstalledDisks {
        targets: Vec::new(),
        _locks: locks,
    };
    for (index, (source, (jailed, target))) in capture.disks.iter().zip(targets).enumerate() {
        if jailed.try_exists()? {
            fs::remove_file(&jailed)?;
        }
        if let Some(attached) = attached {
            let disk = &attached.disks[index];
            fs::hard_link(disk.backing_file(), &jailed)?;
            install_block_device(
                disk.device_path(),
                &jailed.with_extension("ublk"),
                jailer_uid(config, record)?,
            )?;
        } else {
            copy_sparse_reflink(source, &jailed)?;
        }
        fs::set_permissions(&jailed, Permissions::from_mode(0o600))?;
        File::open(&jailed)?.sync_all()?;
        if let Some(target) = target {
            installed.targets.push((jailed, target));
        }
    }
    File::open(root)?.sync_all()?;
    Ok(installed)
}

#[cfg(target_os = "linux")]
fn install_block_device(source: &Path, target: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let metadata = fs::metadata(source)?;
    ensure!(
        metadata.file_type().is_block_device(),
        "external disk is not a block device"
    );
    let path = std::ffi::CString::new(target.as_os_str().as_bytes())?;
    if unsafe { libc::mknod(path.as_ptr(), libc::S_IFBLK | 0o600, metadata.rdev()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    chown(target, Some(uid), Some(uid))?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_block_device(_source: &Path, _target: &Path, _uid: u32) -> Result<()> {
    bail!("external Firecracker block devices require a Linux host")
}

#[cfg(test)]
#[path = "firecracker_filesystem_tests.rs"]
mod tests;
