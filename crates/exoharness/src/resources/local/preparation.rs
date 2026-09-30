use super::*;

pub fn git_preparation_command(
    definition: &ResourceDefinition,
    mut environment: HashMap<String, String>,
    depth: std::num::NonZeroU32,
    timeout: std::time::Duration,
) -> Result<crate::SandboxCommand> {
    definition.validate()?;
    let ResourceSource::GitRepository {
        url: Some(url),
        checkout,
        ..
    } = &definition.source
    else {
        bail!("sandbox Git preparation requires a remote repository");
    };
    ensure!(
        !timeout.is_zero(),
        "Git preparation timeout must be positive"
    );
    let (kind, reference) = match checkout {
        Some(GitCheckout::Branch { name }) => ("branch", name.as_str()),
        Some(GitCheckout::Commit { sha }) => ("commit", sha.as_str()),
        None => ("default", ""),
    };
    environment.extend([
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GIT_LFS_SKIP_SMUDGE".into(), "1".into()),
    ]);
    Ok(crate::SandboxCommand {
        argv: vec![
            "/bin/sh".into(),
            "-ceu".into(),
            include_str!("prepare_git.sh").into(),
            "exo-git".into(),
            url.clone(),
            kind.into(),
            reference.into(),
            depth.to_string(),
        ],
        env: environment,
        display_argv: None,
        cwd: Some(definition.mount_path.clone()),
        timeout: Some(timeout),
    })
}

pub struct ResourcePreparation {
    pub scope: ResourceScope,
    pub mount: FileSystemMount,
}

impl ResourceStore {
    pub fn prepare_volume(
        &self,
        definition: ResourceDefinition,
        identity: &str,
        populate: impl FnOnce(ResourcePreparation) -> Result<()>,
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
        let prepared = populate(ResourcePreparation {
            scope: ResourceScope::Thread {
                agent_id,
                thread_id,
            },
            mount: FileSystemMount {
                host_path: host_path.to_string_lossy().into_owned(),
                mount_path: definition.mount_path.clone(),
                mode: crate::FileSystemMountMode::ReadWrite,
                internal: Some(true),
            },
        });
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
