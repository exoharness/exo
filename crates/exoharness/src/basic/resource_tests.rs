use super::*;
use std::time::Duration;

use crate::resources::{
    MaterializeResourcesRequest, PreparedResource, ResourceDefinition, ResourceSource,
};
use crate::test_support::local_test_config;
use crate::{EnvironmentDefinition, FileSystemMountMode};
use tokio::sync::Semaphore;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(5);

struct ResourceBackend {
    entered: mpsc::UnboundedSender<(ConversationId, bool)>,
    release: Semaphore,
    materialized: Mutex<HashSet<ConversationId>>,
}

#[async_trait]
impl ManagedSandboxBackend for ResourceBackend {
    async fn materialize_resources(
        &self,
        request: MaterializeResourcesRequest,
    ) -> Result<Vec<FileSystemMount>> {
        self.entered.send((request.thread, request.resume))?;
        self.release.acquire().await?.forget();
        self.materialized.lock().unwrap().insert(request.thread);
        Ok(Vec::new())
    }

    async fn remove_thread_resources(&self, _agent: AgentId, thread: ConversationId) -> Result<()> {
        assert!(self.materialized.lock().unwrap().remove(&thread));
        Ok(())
    }

    fn is_local(&self) -> bool {
        false
    }

    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        &[]
    }

    async fn acquire(&self, _request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("resource test does not start VMs")
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("resource test does not attach VMs")
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("resource test does not restore VMs")
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    harness: BasicExoHarness,
    agent: Arc<dyn AgentHandle>,
    backend: Arc<ResourceBackend>,
    entered: mpsc::UnboundedReceiver<(ConversationId, bool)>,
}

// Unblock detached materializers even if an assertion fails.
impl Drop for Fixture {
    fn drop(&mut self) {
        self.backend.release.close();
    }
}

impl Fixture {
    async fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let (entered, receiver) = mpsc::unbounded_channel();
        let backend = Arc::new(ResourceBackend {
            entered,
            release: Semaphore::new(0),
            materialized: Mutex::new(HashSet::new()),
        });
        let mut config = local_test_config(directory.path());
        config
            .sandbox_backends
            .push(SandboxBackendRegistration::from_backend(
                SandboxProvider::Firecracker,
                backend.clone(),
            ));
        let harness = BasicExoHarness::new(config).await?;
        let agent = harness
            .new_agent(NewAgentRequest {
                slug: "resources".into(),
                name: "Resources".into(),
                vaults: vec![],
            })
            .await?;
        Ok(Self {
            directory,
            harness,
            agent,
            backend,
            entered: receiver,
        })
    }

    async fn thread(&self) -> Result<Arc<dyn ConversationHandle>> {
        timeout(DEADLINE, self.agent.new_conversation(Default::default())).await?
    }

    fn resources(&self) -> Vec<PreparedResource> {
        // The test backend gates materialization without cloning a repo or disk.
        vec![PreparedResource {
            definition: ResourceDefinition {
                name: "workspace".into(),
                mount_path: "/workspace".into(),
                mode: FileSystemMountMode::ReadWrite,
                source: ResourceSource::Directory {
                    path: self.directory.path().to_owned(),
                },
            },
            snapshot: None,
        }]
    }

    fn prepare(
        &self,
        thread: Arc<dyn ConversationHandle>,
    ) -> JoinHandle<Result<Vec<FileSystemMount>>> {
        let resources = self.resources();
        tokio::spawn(async move {
            thread
                .materialize_resources(resources, SandboxProvider::Firecracker)
                .await
        })
    }

    async fn next(&mut self) -> Result<(ConversationId, bool)> {
        timeout(DEADLINE, self.entered.recv())
            .await?
            .context("materializer did not start")
    }

    fn resources_exist(&self, thread: ConversationId) -> bool {
        self.directory
            .path()
            .join("resources/threads")
            .join(self.agent.record().id.to_string())
            .join(thread.to_string())
            .exists()
    }
}

async fn write_metadata(thread: &dyn ConversationHandle) -> Result<()> {
    timeout(
        DEADLINE,
        thread.write_artifact(WriteArtifactRequest {
            path: "config/test.json".into(),
            contents: b"{}".to_vec(),
        }),
    )
    .await??;
    Ok(())
}

async fn assert_pending<T>(task: &mut JoinHandle<T>) {
    assert!(
        timeout(Duration::from_millis(50), task).await.is_err(),
        "operation raced resource preparation"
    );
}

#[tokio::test]
async fn resource_preparation_allows_metadata_writes_and_other_threads() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let first = fixture.thread().await?;
    let preparation = fixture.prepare(first.clone());
    assert_eq!(fixture.next().await?, (first.record().id, false));

    write_metadata(first.as_ref()).await?;
    let second = fixture.thread().await?;
    let other_preparation = fixture.prepare(second.clone());
    assert_eq!(fixture.next().await?, (second.record().id, false));

    fixture.backend.release.add_permits(2);
    timeout(DEADLINE, preparation).await???;
    timeout(DEADLINE, other_preparation).await???;
    Ok(())
}

#[tokio::test]
async fn resource_preparation_serializes_the_same_thread_and_rechecks_resume() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let thread = fixture.thread().await?;
    let preparation = fixture.prepare(thread.clone());
    assert_eq!(fixture.next().await?, (thread.record().id, false));
    let mut resumed = fixture.prepare(thread.clone());
    assert_pending(&mut resumed).await;
    assert!(fixture.entered.try_recv().is_err());
    write_metadata(thread.as_ref()).await?;

    fixture.backend.release.add_permits(1);
    timeout(DEADLINE, preparation).await???;
    assert_eq!(fixture.next().await?, (thread.record().id, true));
    fixture.backend.release.add_permits(1);
    timeout(DEADLINE, resumed).await???;
    Ok(())
}

#[tokio::test]
async fn thread_deletion_waits_for_cancelled_preparation_and_removes_resources() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let thread = fixture.thread().await?;
    let id = thread.record().id;
    let preparation = fixture.prepare(thread.clone());
    assert_eq!(fixture.next().await?, (id, false));
    preparation.abort();
    assert!(preparation.await.unwrap_err().is_cancelled());

    let agent = fixture.agent.clone();
    let mut deletion = tokio::spawn(async move { agent.delete_conversation(&id).await });
    assert_pending(&mut deletion).await;
    let other = fixture.thread().await?;
    write_metadata(other.as_ref()).await?;

    fixture.backend.release.add_permits(1);
    assert!(timeout(DEADLINE, deletion).await???);
    assert!(fixture.agent.get_conversation(&id).await?.is_none());
    assert!(!fixture.resources_exist(id));
    assert!(fixture.backend.materialized.lock().unwrap().is_empty());

    assert!(
        thread
            .materialize_resources(fixture.resources(), SandboxProvider::Firecracker)
            .await
            .is_err()
    );
    assert!(!fixture.resources_exist(id));
    assert!(fixture.entered.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn agent_deletion_waits_for_all_preparation_without_blocking_other_agents() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let first = fixture.thread().await?;
    let second = fixture.thread().await?;
    let mut first_preparation = fixture.prepare(first.clone());
    assert_eq!(fixture.next().await?, (first.record().id, false));
    let mut second_preparation = fixture.prepare(second.clone());
    assert_eq!(fixture.next().await?, (second.record().id, false));

    let harness = fixture.harness.clone();
    let id = fixture.agent.record().id;
    let mut deletion = tokio::spawn(async move { harness.delete_agent(&id).await });
    assert_pending(&mut deletion).await;
    timeout(
        DEADLINE,
        fixture.harness.new_agent(NewAgentRequest {
            slug: "other".into(),
            name: "Other".into(),
            vaults: vec![],
        }),
    )
    .await??;

    fixture.backend.release.add_permits(1);
    let first_completed = timeout(DEADLINE, async {
        tokio::select! {
            result = &mut first_preparation => { result??; Ok::<_, anyhow::Error>(true) }
            result = &mut second_preparation => { result??; Ok(false) }
        }
    })
    .await??;
    assert_pending(&mut deletion).await;
    fixture.backend.release.add_permits(1);
    if first_completed {
        timeout(DEADLINE, second_preparation).await???;
    } else {
        timeout(DEADLINE, first_preparation).await???;
    }
    assert!(timeout(DEADLINE, deletion).await???);
    assert!(fixture.harness.get_agent(&id).await?.is_none());
    assert!(!fixture.resources_exist(first.record().id));
    assert!(!fixture.resources_exist(second.record().id));
    assert!(fixture.backend.materialized.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn environment_changes_and_forks_wait_for_resource_preparation() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let thread = fixture.thread().await?;
    let preparation = fixture.prepare(thread.clone());
    assert_eq!(fixture.next().await?, (thread.record().id, false));

    let changing = thread.clone();
    let mut update = tokio::spawn(async move {
        changing
            .update_environment(EnvironmentDefinition {
                name: "local".into(),
                config: CreateSandboxRequest {
                    provider: SandboxProvider::LocalProcess,
                    image: "test".into(),
                    tcp_ports: vec![],
                    name: None,
                    resources: None,
                    default_workdir: None,
                    file_system_mounts: None,
                    durable_file_systems: None,
                    policy: None,
                    enable_networking: Some(true),
                    idle_seconds: None,
                },
            })
            .await
    });
    let forking = thread.clone();
    let mut fork = tokio::spawn(async move { forking.fork(Default::default()).await });
    assert_pending(&mut update).await;
    assert_pending(&mut fork).await;
    write_metadata(thread.as_ref()).await?;

    fixture.backend.release.add_permits(1);
    timeout(DEADLINE, preparation).await???;
    assert!(timeout(DEADLINE, update).await??.is_err());
    assert!(timeout(DEADLINE, fork).await??.is_err());
    Ok(())
}
