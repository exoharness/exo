use super::*;

impl ResourceStore {
    pub fn prepare_volume(
        &self,
        definition: ResourceDefinition,
        identity: &str,
        populate: impl FnOnce(ResourceScope, FileSystemMount) -> Result<()>,
    ) -> Result<PreparedResource> {
        definition.validate()?;
        ensure!(!identity.is_empty(), "resource cache identity is required");
        self.initialize()?;
        let key = digest(&serde_json::to_vec(&(
            &definition.source,
            identity,
            self.volume_size_gib,
        ))?);
        let _lock = self.lock(&key)?;
        let fresh = !self.root.join("snapshots").join(&key).exists();
        self.publish(&key, |scope, volume| {
            // Firecracker opens the disk itself, so detach it before starting the VM.
            // Only new, empty disks need host-side ownership setup; cached guest data
            // must not be mounted on the host to refresh it.
            let host_path = if self.image_size_gib.is_some() {
                if fresh {
                    self.prepare_ownership(&self.mount_volume(volume)?)?;
                    self.unmount_volume(volume)?;
                }
                volume.to_owned()
            } else {
                self.mount_volume(volume)?
            };
            populate(
                scope,
                FileSystemMount {
                    host_path: host_path.to_string_lossy().into_owned(),
                    mount_path: definition.mount_path.clone(),
                    mode: crate::FileSystemMountMode::ReadWrite,
                    internal: Some(true),
                },
            )
        })?;
        Ok(PreparedResource {
            definition,
            snapshot: Some(key),
        })
    }

    pub fn remove_snapshot(&self, snapshot: &str) -> Result<()> {
        validate_key(snapshot)?;
        let _lock = self.lock(snapshot)?;
        let directory = self.root.join("snapshots").join(snapshot);
        match fs::remove_dir_all(directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
