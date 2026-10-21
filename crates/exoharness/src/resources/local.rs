use crate::resources::git_progress;
use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{GitCredential, PreparedResource, ResourceDefinition, ResourceSource};
use crate::local_volume::{clone_volume, create_volume, mount_volume, unmount_volume};
use crate::{AgentId, FileSystemMount, ResourceScope, ThreadId};

#[derive(Clone)]
pub struct ResourceStore {
    root: PathBuf,
    excluded: PathBuf,
    master_key: Option<PathBuf>,
    image_size_gib: Option<u64>,
    volume_size_gib: Option<u64>,
    cancel: Option<tokio_util::sync::CancellationToken>,
}

#[derive(Serialize, Deserialize)]
struct Instance {
    prepared: PreparedResource,
    snapshot: String,
    revision: Option<String>,
}

impl ResourceStore {
    pub fn new(root: &Path) -> Result<Self> {
        let root = canonical_path(&std::env::current_dir()?.join(root))?;
        Ok(Self {
            root: root.join("resources"),
            excluded: root,
            master_key: None,
            image_size_gib: None,
            volume_size_gib: None,
            cancel: None,
        })
    }

    pub(crate) fn with_cancellation(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    fn git<I, S>(&self, cwd: &Path, args: I, credential: Option<&GitCredential>) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        git_with_cancellation(cwd, args, credential, self.cancel.as_ref(), None)
    }

    pub fn with_volume_size(mut self, size_gib: u64) -> Result<Self> {
        ensure!(size_gib > 0, "resource volume size must be positive");
        ensure!(
            cfg!(target_os = "macos") || self.image_size_gib.is_some(),
            "bounded resources require APFS volumes or Firecracker resource images"
        );
        self.volume_size_gib = Some(size_gib);
        if self.image_size_gib.is_some() {
            self.image_size_gib = Some(size_gib);
        }
        Ok(self)
    }

    pub(crate) fn excluding_master_key(mut self, path: Option<PathBuf>) -> Result<Self> {
        self.master_key = path.map(|p| canonical_path(&p)).transpose()?;
        Ok(self)
    }

    fn initialize(&self) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        }
        fs::create_dir_all(self.root.join("snapshots"))?;
        Ok(())
    }

    pub fn prepare(&self, resources: Vec<ResourceDefinition>) -> Result<Vec<PreparedResource>> {
        super::validate_resources(&resources)?;
        if resources.is_empty() {
            return Ok(Vec::new());
        }
        self.initialize()?;
        resources.into_iter().map(|definition| {
            let snapshot = if let Some(source) = definition.local_path() {
                ensure!(source.is_absolute() && source.is_dir(), "resource source must be an absolute directory");
                ensure!(!source.starts_with(&self.excluded), "cannot use Exo state as a resource");
                if let Some(key) = &self.master_key {
                    ensure!(!key.starts_with(source), "resource source contains the vault master key");
                }
                let before = self.fingerprint(source)?;
                let key = digest(&serde_json::to_vec(&(&definition.source, &before))?);
                let _lock = self.lock(&key)?;
                let destination = self.root.join("snapshots").join(&key);
                if !destination.exists() {
                    tracing::info!(resource = definition.name, "preparing filesystem resource");
                    self.publish(&key, |_, volume| {
                        let workspace = self.mount_volume(volume)?;
                        if matches!(definition.source, ResourceSource::GitRepository { .. }) {
                            local_git(source, &workspace, &self.excluded)?;
                        } else {
                            copy_directory(source, &workspace, &self.excluded, false)?;
                        }
                        ensure!(self.fingerprint(source)? == before, "resource changed during preparation; retry when writes have finished");
                        self.prepare_ownership(&workspace)
                    })?;
                }
                Some(key)
            } else { None };
            Ok(PreparedResource { definition, snapshot })
        }).collect()
    }

    pub fn materialize(
        &self,
        agent: AgentId,
        thread: ThreadId,
        resources: Vec<PreparedResource>,
        credentials: Vec<Option<GitCredential>>,
    ) -> Result<Vec<FileSystemMount>> {
        if resources.is_empty() {
            return Ok(Vec::new());
        }
        ensure!(
            resources.len() == credentials.len(),
            "resource credentials are incomplete"
        );
        self.initialize()?;
        let _lock = self.lock(&thread.to_string())?;
        let directory = self.thread_directory(agent, thread);
        let manifest = directory.join("resources.json");
        let instances: Vec<Instance> = if manifest.exists() {
            let mut instances: Vec<Instance> = serde_json::from_slice(&fs::read(&manifest)?)?;
            ensure!(
                instances.len() == resources.len()
                    && instances
                        .iter()
                        .zip(&resources)
                        .all(|(instance, resource)| instance.prepared.same_workspace(resource)),
                "cannot change the resources of an existing thread"
            );
            if instances.iter().map(|i| &i.prepared).ne(resources.iter()) {
                for (instance, resource) in instances.iter_mut().zip(resources) {
                    instance.prepared = resource;
                }
                let mut updated = tempfile::NamedTempFile::new_in(&directory)?;
                serde_json::to_writer(updated.as_file_mut(), &instances)?;
                updated.persist(&manifest)?;
            }
            instances
        } else {
            let parent = directory.parent().context("thread resource parent")?;
            fs::create_dir_all(parent)?;
            let staging = tempfile::tempdir_in(parent)?;
            let mut instances = Vec::new();
            for (prepared, credential) in resources.into_iter().zip(credentials) {
                let target = staging.path().join(&prepared.definition.name);
                fs::create_dir(&target)?;
                let (snapshot, revision) = match &prepared.snapshot {
                    Some(snapshot) => {
                        validate_key(snapshot)?;
                        let _lock = self.lock(snapshot)?;
                        self.clone_volume(&self.root.join("snapshots").join(snapshot), &target)?;
                        (snapshot.clone(), None)
                    }
                    None => {
                        self.clone_git_resource(&prepared.definition, credential.as_ref(), &target)?
                    }
                };
                instances.push(Instance {
                    prepared,
                    snapshot,
                    revision,
                });
            }
            fs::write(
                staging.path().join("resources.json"),
                serde_json::to_vec(&instances)?,
            )?;
            fs::rename(staging.path(), &directory)?;
            instances
        };
        instances
            .iter()
            .map(|instance| {
                let definition = &instance.prepared.definition;
                let resource = directory.join(&definition.name);
                let path = if self.image_size_gib.is_some() {
                    resource.clone()
                } else {
                    tracing::info!(target: "exoharness::progress", "Mounting resource {}", definition.name);
                    self.mount_volume(&resource)?
                };
                Ok(FileSystemMount {
                    host_path: path.to_string_lossy().into_owned(),
                    mount_path: definition.mount_path().into_owned(),
                    mode: definition.mode,
                    internal: Some(true),
                })
            })
            .collect()
    }

    pub fn command_env(
        &self,
        scope: ResourceScope,
        mounts: &[FileSystemMount],
        mut env: HashMap<String, String>,
    ) -> Result<HashMap<String, String>> {
        let ResourceScope::Thread {
            agent_id,
            thread_id,
        } = scope
        else {
            return Ok(env);
        };
        if !mounts.iter().any(|mount| mount.internal == Some(true)) {
            return Ok(env);
        }
        let directory = self.thread_directory(agent_id, thread_id);
        let resources: Vec<PreparedResource> = if directory.join("materialized.json").exists() {
            serde_json::from_slice(&fs::read(directory.join("materialized.json"))?)?
        } else if directory.join("resources.json").exists() {
            serde_json::from_slice::<Vec<Instance>>(&fs::read(directory.join("resources.json"))?)?
                .into_iter()
                .map(|instance| instance.prepared)
                .collect()
        } else {
            return Ok(env);
        };
        for resource in resources {
            let definition = resource.definition;
            if !matches!(definition.source, ResourceSource::GitRepository { .. })
                || !mounts.iter().any(|mount| {
                    mount.internal == Some(true) && mount.mount_path == definition.mount_path()
                })
            {
                continue;
            }
            ensure!(
                !Path::new(definition.mount_path().as_ref()).ends_with("*"),
                "Git resource mount_path cannot end with '/*': Git treats it as a trust wildcard"
            );
            let mut settings = vec![(
                "safe.directory".to_owned(),
                definition.mount_path().into_owned(),
            )];
            if let ResourceSource::GitRepository {
                url: Some(url),
                credential: Some(_),
                ..
            } = &definition.source
            {
                settings.extend([
                    (format!("credential.{url}.helper"), String::new()),
                    (
                        format!("credential.{url}.helper"),
                        definition.git_credential_helper(),
                    ),
                    (format!("credential.{url}.useHttpPath"), "true".into()),
                ]);
            }
            for (key, value) in settings {
                let count = env
                    .get("GIT_CONFIG_COUNT")
                    .map_or(Ok(0), |value| value.parse::<usize>())
                    .context("invalid GIT_CONFIG_COUNT in sandbox environment")?;
                let next = count.checked_add(1).context("GIT_CONFIG_COUNT overflow")?;
                env.insert(format!("GIT_CONFIG_KEY_{count}"), key);
                env.insert(format!("GIT_CONFIG_VALUE_{count}"), value);
                env.insert("GIT_CONFIG_COUNT".into(), next.to_string());
            }
        }
        Ok(env)
    }

    pub fn remove_thread(&self, agent: AgentId, thread: ThreadId) -> Result<()> {
        let directory = self.thread_directory(agent, thread);
        if !directory.exists() {
            return Ok(());
        }
        let _lock = self.lock(&thread.to_string())?;
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                self.unmount_volume(&entry.path())?;
            }
        }
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    pub(crate) fn has_thread(&self, agent: AgentId, thread: ThreadId) -> bool {
        self.thread_directory(agent, thread)
            .join("resources.json")
            .exists()
            || self
                .thread_directory(agent, thread)
                .join("materialized.json")
                .exists()
    }

    fn thread_directory(&self, agent: AgentId, thread: ThreadId) -> PathBuf {
        self.root
            .join("threads")
            .join(agent.to_string())
            .join(thread.to_string())
    }

    fn lock(&self, name: &str) -> Result<File> {
        let locks = self.root.join("locks");
        fs::create_dir_all(&locks)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(locks.join(name))?;
        if let Some(cancel) = &self.cancel {
            loop {
                ensure!(!cancel.is_cancelled(), "resource preparation canceled");
                match file.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        } else {
            file.lock()?;
        }
        Ok(file)
    }

    fn fingerprint(&self, source: &Path) -> Result<String> {
        let mut hash = Sha256::new();
        fingerprint(source, &self.excluded, &mut hash)?;
        if source.join(".git").is_file() {
            let common = git(
                source,
                ["rev-parse", "--path-format=absolute", "--git-common-dir"],
                None,
            )?;
            fingerprint(Path::new(common.trim()), &self.excluded, &mut hash)?;
            let index = git(
                source,
                ["rev-parse", "--path-format=absolute", "--git-path", "index"],
                None,
            )?;
            fingerprint(Path::new(index.trim()), &self.excluded, &mut hash)?;
        }
        Ok(format!("{:x}", hash.finalize()))
    }

    fn publish(
        &self,
        key: &str,
        populate: impl FnOnce(ResourceScope, &Path) -> Result<()>,
    ) -> Result<()> {
        let destination = self.root.join("snapshots").join(key);
        let agent_id = crate::Uuid7::now();
        let thread_id = crate::Uuid7::now();
        let threads = self.root.join("threads");
        fs::create_dir_all(&threads)?;
        // Guest preparation must use the same scoped paths as normal resource mounts.
        let temporary = tempfile::Builder::new()
            .prefix(&agent_id.to_string())
            .rand_bytes(0)
            .tempdir_in(&threads)?;
        let staging = temporary.path().join(thread_id.to_string()).join("volume");
        fs::create_dir_all(&staging)?;
        if destination.exists() {
            self.clone_volume(&destination, &staging)?;
        } else {
            self.create_volume(&staging)?;
        }
        let populated = populate(
            ResourceScope::Thread {
                agent_id,
                thread_id,
            },
            &staging,
        );
        if let Err(error) = self.unmount_volume(&staging) {
            drop(temporary.keep());
            return Err(error)
                .with_context(|| format!("unmounting prepared resource at {}", staging.display()));
        }
        populated?;
        if destination.exists() {
            fs::remove_dir_all(&destination)?;
        }
        fs::rename(&staging, &destination)?;
        Ok(())
    }

    fn fetch_checkout(
        &self,
        workspace: &Path,
        checkout: Option<&str>,
        credential: Option<&GitCredential>,
    ) -> Result<(Option<String>, String)> {
        let (branch, source, target) = match checkout {
            Some(revision)
                if matches!(revision.len(), 40 | 64)
                    && revision.bytes().all(|c| c.is_ascii_hexdigit()) =>
            {
                (None, revision.to_owned(), "refs/exo/checkout".to_owned())
            }
            reference => {
                let refs = match reference {
                    Some(name) => self.git(
                        workspace,
                        [
                            "ls-remote",
                            "origin",
                            &format!("refs/heads/{name}"),
                            &format!("refs/tags/{name}"),
                        ],
                        credential,
                    )?,
                    None => self.git(
                        workspace,
                        ["ls-remote", "--symref", "origin", "HEAD"],
                        credential,
                    )?,
                };
                let branch = match reference {
                    Some(name) => refs
                        .lines()
                        .any(|line| line.ends_with(&format!("\trefs/heads/{name}")))
                        .then(|| name.to_owned()),
                    None => Some(
                        refs.lines()
                            .find_map(|line| {
                                line.strip_prefix("ref: refs/heads/")?
                                    .strip_suffix("\tHEAD")
                            })
                            .context("Git remote has no default branch")?
                            .to_owned(),
                    ),
                };
                let advertised = match reference {
                    None => "HEAD".to_owned(),
                    Some(name) if branch.is_some() => format!("refs/heads/{name}"),
                    Some(name) => format!("refs/tags/{name}"),
                };
                let revision = refs
                    .lines()
                    .find_map(|line| {
                        let (revision, name) = line.split_once('\t')?;
                        (name == advertised && !revision.starts_with("ref: ")).then_some(revision)
                    })
                    .context("Git remote does not contain the requested checkout")?;
                ensure!(
                    matches!(revision.len(), 40 | 64)
                        && revision.bytes().all(|c| c.is_ascii_hexdigit()),
                    "invalid Git revision"
                );
                let target = match &branch {
                    Some(name) => format!("refs/remotes/origin/{name}"),
                    None => advertised,
                };
                (branch, revision.to_owned(), target)
            }
        };
        let refspec = format!("+{source}:{target}");
        self.git(
            workspace,
            [
                "fetch",
                "--progress",
                "--force",
                "--depth=1",
                "--no-tags",
                "--no-recurse-submodules",
                "origin",
                &refspec,
            ],
            credential,
        )?;
        let fetch = match &branch {
            Some(name) => format!("+refs/heads/{name}:{target}"),
            None => refspec,
        };
        self.git(workspace, ["config", "remote.origin.fetch", &fetch], None)?;
        let revision = self
            .git(
                workspace,
                ["rev-parse", "--verify", "FETCH_HEAD^{commit}"],
                None,
            )?
            .trim()
            .to_owned();
        Ok((branch, revision))
    }

    fn clone_git_resource(
        &self,
        definition: &ResourceDefinition,
        credential: Option<&GitCredential>,
        target: &Path,
    ) -> Result<(String, Option<String>)> {
        let ResourceSource::GitRepository {
            url: Some(url),
            checkout,
            ..
        } = &definition.source
        else {
            bail!("local resource has no prepared snapshot");
        };
        let parsed_url = url::Url::parse(url)?;
        let repository = parsed_url[url::Position::BeforeHost..url::Position::AfterPath]
            .trim_end_matches('/')
            .trim_end_matches(".git");
        let key = digest(&serde_json::to_vec(&(
            "shallow-v1",
            url,
            checkout,
            credential.map(|c| &c.identity),
        ))?);
        let _lock = self.lock(&key)?;
        let cache = self.root.join("git").join(&key);
        if !cache.exists() {
            let parent = cache.parent().context("Git cache parent")?;
            fs::create_dir_all(parent)?;
            let staging = tempfile::tempdir_in(parent)?;
            self.create_volume(staging.path())?;
            fs::rename(staging.path(), &cache)?;
        }
        tracing::info!(target: "exoharness::progress", "Mounting Git cache for {}", repository);
        let workspace = self.mount_volume(&cache)?;
        let updated = (|| {
            if !workspace.join(".git").exists() {
                self.git(&workspace, ["init"], None)?;
                self.git(&workspace, ["remote", "add", "origin", url], None)?;
            }
            tracing::info!(target: "exoharness::progress", "Fetching Git resource {}", repository);
            let (branch, revision) =
                self.fetch_checkout(&workspace, checkout.as_deref(), credential)?;
            if let Some(name) = branch {
                git_with_cancellation(
                    &workspace,
                    ["checkout", "--progress", "--force", "-B", &name, &revision],
                    None,
                    self.cancel.as_ref(),
                    Some(repository),
                )?;
                self.git(
                    &workspace,
                    ["config", &format!("branch.{name}.remote"), "origin"],
                    None,
                )?;
                self.git(
                    &workspace,
                    [
                        "config",
                        &format!("branch.{name}.merge"),
                        &format!("refs/heads/{name}"),
                    ],
                    None,
                )?;
            } else {
                git_with_cancellation(
                    &workspace,
                    ["checkout", "--progress", "--force", "--detach", &revision],
                    None,
                    self.cancel.as_ref(),
                    Some(repository),
                )?;
            }
            Ok::<_, anyhow::Error>(revision)
        })();
        let updated = updated.and_then(|revision| {
            tracing::info!(target: "exoharness::progress", "Preparing Git workspace for {}", repository);
            self.prepare_ownership(&workspace)?;
            Ok(revision)
        });
        self.unmount_volume(&cache)?;
        let revision = updated?;
        tracing::info!(target: "exoharness::progress", "Cloning Git workspace for {}", repository);
        self.clone_volume(&cache, target)?;
        Ok((key, Some(revision)))
    }
}

fn validate_key(key: &str) -> Result<()> {
    ensure!(
        key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid resource snapshot"
    );
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn canonical_path(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let parent = canonical_path(path.parent().context("path has no existing ancestor")?)?;
    Ok(parent.join(path.file_name().context("path has no filename")?))
}

fn fingerprint(path: &Path, excluded: &Path, hash: &mut Sha256) -> Result<()> {
    if path == excluded {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update(metadata.len().to_le_bytes());
    hash.update(
        metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
            .to_le_bytes(),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        hash.update(metadata.mode().to_le_bytes());
    }
    if metadata.is_symlink() {
        hash.update(fs::read_link(path)?.as_os_str().as_encoded_bytes());
    }
    if metadata.is_dir() {
        let mut entries = fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        for entry in entries {
            fingerprint(&entry, excluded, hash)?;
        }
    }
    Ok(())
}

fn copy_directory(source: &Path, target: &Path, excluded: &Path, skip_git: bool) -> Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.path() == excluded || (skip_git && entry.file_name() == ".git") {
            continue;
        }
        let destination = target.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_directory(&entry.path(), &destination, excluded, false)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, destination)?;
            #[cfg(not(unix))]
            bail!("filesystem resources require macOS or Linux");
        } else {
            ensure!(
                kind.is_file(),
                "resource contains a special file: {}",
                entry.path().display()
            );
            fs::copy(entry.path(), destination)?;
        }
    }
    fs::set_permissions(target, fs::metadata(source)?.permissions())?;
    Ok(())
}

fn local_git(source: &Path, target: &Path, excluded: &Path) -> Result<()> {
    ensure!(
        source.join(".git").exists(),
        "local Git resource must be a working tree root"
    );
    clone_git(source, target)?;
    let revision = git(source, ["rev-parse", "HEAD"], None)?;
    git(target, ["update-ref", "HEAD", revision.trim()], None)?;
    let index = git(
        source,
        ["rev-parse", "--path-format=absolute", "--git-path", "index"],
        None,
    )?;
    fs::copy(index.trim(), target.join(".git/index"))?;
    let remotes = git(source, ["remote"], None)?;
    if remotes.lines().any(|remote| remote == "origin") {
        let origin = git(source, ["remote", "get-url", "origin"], None)?;
        let origin = origin.trim();
        if let Ok(url) = url::Url::parse(origin) {
            ensure!(
                url.password().is_none() && (url.scheme() != "https" || url.username().is_empty()),
                "remove embedded credentials from the repository origin before importing it"
            );
        }
        git(target, ["remote", "set-url", "origin", origin], None)?;
    } else {
        git(target, ["remote", "remove", "origin"], None)?;
    }
    copy_directory(source, target, excluded, true)
}

fn clone_git(source: &Path, target: &Path) -> Result<()> {
    git(
        target.parent().context("Git clone parent")?,
        [
            OsStr::new("clone"),
            OsStr::new("--no-hardlinks"),
            OsStr::new("--no-checkout"),
            OsStr::new("--"),
            source.as_os_str(),
            target.as_os_str(),
        ],
        None,
    )?;
    Ok(())
}

fn git_command(cwd: &Path, credential: Option<&GitCredential>) -> (Command, Option<String>) {
    use base64::Engine;
    let mut command = Command::new("git");
    command.current_dir(cwd);
    if credential.is_some() {
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/local/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "credential.helper="]);
    }
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_SHALLOW_FILE",
    ] {
        command.env_remove(variable);
    }
    command
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1")
        .env("GIT_HTTP_LOW_SPEED_TIME", "30")
        .args(["-c", &format!("safe.directory={}", cwd.display())])
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "http.followRedirects=false",
            "-c",
            "filter.lfs.required=false",
            "-c",
            "filter.lfs.smudge=",
            "-c",
            "filter.lfs.process=",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.file.allow=always",
        ]);
    let header = credential.map(|c| {
        format!(
            "Authorization: Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", c.username, c.token))
        )
    });
    if let Some(header) = &header {
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", header);
    }
    (command, header)
}

fn git<I, S>(cwd: &Path, args: I, credential: Option<&GitCredential>) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_with_cancellation(cwd, args, credential, None, None)
}

fn git_with_cancellation<I, S>(
    cwd: &Path,
    args: I,
    credential: Option<&GitCredential>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    progress: Option<&str>,
) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (mut command, header) = git_command(cwd, credential);
    command.args(args);
    let output = command_output(command, cancel, progress)
        .context("running Git for resource preparation")?;
    if !output.status.success() {
        let mut message = String::from_utf8_lossy(&output.stderr).into_owned();
        if let Some(c) = credential {
            message = message.replace(&c.token, "[redacted]");
        }
        if let Some(header) = header {
            message = message.replace(&header, "[redacted]");
        }
        if message.contains("terminal prompts disabled")
            || message.contains("Authentication failed")
            || message.contains("returned error: 401")
            || message.contains("returned error: 403")
        {
            let help = if credential.is_some() {
                "Check the repository URL and the vault credential's repository access."
            } else {
                "Check the repository URL and your Git credentials on the runtime host, or set credential to a secret in a selected vault."
            };
            bail!("preparing Git resource: {}\n{help}", message.trim());
        }
        bail!("preparing Git resource: {}", message.trim());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn command_output(
    mut command: Command,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    progress: Option<&str>,
) -> Result<Output> {
    let Some(cancel) = cancel else {
        return Ok(command.output()?);
    };
    ensure!(!cancel.is_cancelled(), "resource preparation canceled");
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    tokio::runtime::Handle::current().block_on(async {
        let mut child = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()?;
        #[cfg(unix)]
        let process_group = child.id().context("resource command process ID")? as i32;
        let mut stdout = child.stdout.take().context("resource command stdout")?;
        let mut stderr = BufReader::new(child.stderr.take().context("resource command stderr")?);
        let mut output = Vec::new();
        let mut errors = Vec::new();
        tokio::select! {
            result = async {
                let (status, _, _) = tokio::try_join!(
                    child.wait(),
                    stdout.read_to_end(&mut output),
                    async {
                        let mut record = Vec::new();
                        // Git rewrites progress lines with carriage returns.
                        while stderr.read_until(b'\r', &mut record).await? > 0 {
                            errors.extend_from_slice(&record);
                            if let Some(resource) = progress {
                                for line in record.split(|byte| *byte == b'\n' || *byte == b'\r') {
                                    if let Some((phase, percent)) = git_progress(line) {
                                        tracing::info!(target: "exoharness::progress", "{phase} {resource}: {percent}%");
                                    }
                                }
                            }
                            record.clear();
                        }
                        Ok::<_, std::io::Error>(())
                    },
                )?;
                Ok(Output { status, stdout: output, stderr: errors })
            } => result,
            _ = cancel.cancelled() => {
                #[cfg(unix)]
                if unsafe { libc::kill(-process_group, libc::SIGKILL) } != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        return Err(error.into());
                    }
                }
                child.kill().await?;
                bail!("resource preparation canceled");
            }
        }
    })
}

fn checked(command: &mut Command) -> Result<Output> {
    let output = command
        .output()
        .context("running filesystem resource command")?;
    ensure!(
        output.status.success(),
        "filesystem resource command failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output)
}

#[cfg(test)]
mod tests;

mod images;
mod preparation;
pub(crate) use images::host_git_credential;
