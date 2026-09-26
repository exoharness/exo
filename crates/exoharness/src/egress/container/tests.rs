use super::*;
use anyhow::bail;
use futures::poll;
use tokio::sync::{mpsc, oneshot};

struct NoCredentials;

#[async_trait]
impl EgressCredentialResolver for NoCredentials {
    async fn resolve(
        &self,
        _identity: &EgressIdentity,
        _name: &str,
        _destination: &super::super::EgressDestination,
    ) -> Result<String> {
        bail!("not used")
    }
}

struct BlockingBackend(mpsc::UnboundedSender<(&'static str, String, oneshot::Sender<()>)>);

#[async_trait]
impl ManagedSandboxBackend for BlockingBackend {
    fn is_local(&self) -> bool {
        true
    }
    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        &[]
    }
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let (release, wait) = oneshot::channel();
        self.0
            .send(("acquire", request.sandbox_id.clone(), release))?;
        wait.await?;
        crate::LocalProcessSandboxBackend::new()
            .acquire(request)
            .await
    }
    async fn terminate(&self, request: SandboxRequest) -> Result<()> {
        let (release, wait) = oneshot::channel();
        self.0.send(("terminate", request.sandbox_id, release))?;
        wait.await?;
        Ok(())
    }
    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("not used")
    }
    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("not used")
    }
}

fn request(id: &str) -> SandboxRequest {
    SandboxRequest {
        sandbox_id: id.into(),
        scope: Default::default(),
        spec: crate::SandboxSpec {
            image: "test".into(),
            resources: None,
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            policy: SandboxNetworkPolicy::Unrestricted.into(),
            default_workdir: "/".into(),
        },
        lifecycle: crate::SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(60)),
        },
        provider_state: None,
    }
}

#[tokio::test]
async fn unrelated_acquires_progress_while_same_sandbox_and_termination_wait() -> Result<()> {
    let (calls, mut received) = mpsc::unbounded_channel();
    let backend = CredentialContainerBackend {
        inner: Arc::new(BlockingBackend(calls)),
        provider: SandboxProvider::Docker,
        resolver: Arc::new(NoCredentials),
        sandboxes: Mutex::new(HashMap::new()),
    };
    let mut first = Box::pin(backend.acquire(request("first")));
    let mut same = Box::pin(backend.acquire(request("first")));
    let mut other = Box::pin(backend.acquire(request("other")));
    let mut terminate = Box::pin(backend.terminate(request("first")));
    assert!(poll!(first.as_mut()).is_pending());
    let (action, id, release_first) = received.try_recv()?;
    assert_eq!((action, id.as_str()), ("acquire", "first"));
    assert!(poll!(same.as_mut()).is_pending());
    assert!(poll!(other.as_mut()).is_pending());
    let (action, id, release_other) = received.try_recv()?;
    assert_eq!((action, id.as_str()), ("acquire", "other"));
    assert!(poll!(terminate.as_mut()).is_pending());
    assert!(received.try_recv().is_err());

    release_other.send(()).unwrap();
    other.await?;
    release_first.send(()).unwrap();
    first.await?;
    assert!(poll!(same.as_mut()).is_pending());
    let (action, id, release_same) = received.try_recv()?;
    assert_eq!((action, id.as_str()), ("acquire", "first"));
    assert!(poll!(terminate.as_mut()).is_pending());
    assert!(received.try_recv().is_err());
    release_same.send(()).unwrap();
    same.await?;
    assert!(poll!(terminate.as_mut()).is_pending());
    let (action, id, release_terminate) = received.try_recv()?;
    assert_eq!((action, id.as_str()), ("terminate", "first"));
    release_terminate.send(()).unwrap();
    terminate.await?;
    assert!(!backend.sandboxes.lock().unwrap().contains_key("first"));
    Ok(())
}

#[tokio::test]
async fn failed_protected_acquires_release_entries_and_drop_tolerates_lock_errors() -> Result<()> {
    let (calls, mut received) = mpsc::unbounded_channel();
    let backend = CredentialContainerBackend {
        inner: Arc::new(BlockingBackend(calls)),
        provider: SandboxProvider::Docker,
        resolver: Arc::new(NoCredentials),
        sandboxes: Mutex::new(HashMap::new()),
    };
    let mut protected = request("protected");
    protected
        .spec
        .policy
        .credentials
        .push(crate::EgressCredentialBinding {
            name: "key".into(),
            environment_variable: "API_KEY".into(),
            model: None,
            networking: crate::CredentialNetworkPolicy::Limited {
                allowed_hosts: vec!["api.example.com".into()],
            },
            injection_location: crate::CredentialInjectionLocation { header: true },
        });
    let mut invalid = protected.clone();
    invalid.spec.policy.networking = SandboxNetworkPolicy::Disabled;
    assert!(backend.acquire(invalid).await.is_err());
    assert!(received.try_recv().is_err());
    assert!(backend.sandboxes.lock().unwrap().is_empty());

    let mut failed = Box::pin(backend.acquire(protected));
    assert!(poll!(failed.as_mut()).is_pending());
    let (action, id, release) = received.try_recv()?;
    assert_eq!((action, id.as_str()), ("acquire", "protected"));
    drop(release);
    assert!(failed.await.is_err());
    assert!(backend.sandboxes.lock().unwrap().is_empty());

    let entry = backend.sandbox_entry("locked");
    let _current = entry.try_lock()?;
    std::thread::scope(|scope| {
        let poison = scope.spawn(|| {
            let _guard = backend.sandboxes.lock().unwrap();
            panic!("poison registry");
        });
        assert!(poison.join().is_err());
    });
    drop(backend);
    Ok(())
}
