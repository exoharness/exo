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
        let snapshots = self.root.join("snapshots");
        let destination = snapshots.join(&key);
        let agent_id = crate::Uuid7::now();
        let thread_id = crate::Uuid7::now();
        let agent_name = agent_id.to_string();
        let threads = self.root.join("threads");
        fs::create_dir_all(&threads)?;
        let temporary = tempfile::Builder::new()
            .prefix(&agent_name)
            .rand_bytes(0)
            .tempdir_in(&threads)?;
        let thread = temporary.path().join(thread_id.to_string());
        fs::create_dir(&thread)?;
        let staging = tempfile::tempdir_in(&thread)?;
        if destination.exists() {
            self.clone_volume(&destination, staging.path())?;
        } else {
            self.create_volume(staging.path())?;
            let workspace = self.mount_volume(staging.path())?;
            let ownership = self.prepare_ownership(&workspace);
            self.unmount_volume(staging.path())?;
            ownership?;
        }
        let host_path = if self.image_size_gib.is_some() {
            staging.path().to_owned()
        } else {
            self.mount_volume(staging.path())?
        };
        let prepared = populate(
            ResourceScope::Thread {
                agent_id,
                thread_id,
            },
            FileSystemMount {
                host_path: host_path.to_string_lossy().into_owned(),
                mount_path: definition.mount_path.clone(),
                mode: crate::FileSystemMountMode::ReadWrite,
                internal: Some(true),
            },
        );
        if let Err(error) = self.unmount_volume(staging.path()) {
            let path = staging.keep();
            drop(temporary.keep());
            return Err(error)
                .with_context(|| format!("unmounting prepared resource at {}", path.display()));
        }
        prepared?;
        if destination.exists() {
            fs::remove_dir_all(&destination)?;
        }
        fs::rename(staging.path(), &destination)?;
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
