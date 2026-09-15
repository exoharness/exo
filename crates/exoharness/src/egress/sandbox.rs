use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use anyhow::{Result, ensure};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

use super::{
    EgressCredentialResolver, EgressIdentity, EgressProxy, EgressTransport, State, UpstreamResolver,
};
use crate::{ManagedSandboxHandle, SandboxCommand, SandboxRequest};

const PREPARE_TRUST: &str = r#"set -eu
umask 077
cat /etc/ssl/certs/ca-certificates.crt > "$EXO_EGRESS_CA_PATH"
printf '\n%s\n' "$EXO_EGRESS_CA_PEM" >> "$EXO_EGRESS_CA_PATH"
"#;

// Proxy and guest-side settings for one VM allocation. The handle uses this to
// add placeholders and the CA path to commands; it never passes real secrets.
pub(crate) struct SandboxEgress {
    proxy: EgressProxy,
    ca_path: String,
}

impl SandboxEgress {
    pub(crate) fn endpoints(&self) -> crate::SandboxEgressProxy {
        self.proxy.endpoints()
    }

    pub(crate) async fn initialize(
        &self,
        handle: &dyn ManagedSandboxHandle,
        source: Ipv4Addr,
    ) -> Result<()> {
        self.proxy.bind_source(source).await?;
        let prepared = handle
            .exec(&SandboxCommand {
                argv: vec!["/bin/sh".into(), "-c".into(), PREPARE_TRUST.into()],
                env: HashMap::from([
                    ("EXO_EGRESS_CA_PATH".into(), self.ca_path.clone()),
                    ("EXO_EGRESS_CA_PEM".into(), self.proxy.ca_pem().into()),
                ]),
                display_argv: None,
                cwd: None,
                timeout: Some(Duration::from_secs(30)),
            })
            .await?;
        ensure!(
            prepared.ok,
            "could not prepare sandbox TLS trust: {}",
            prepared.stderr
        );
        Ok(())
    }

    pub(crate) fn prepare_command<'a>(
        egress: Option<&Self>,
        command: &'a SandboxCommand,
    ) -> Result<Cow<'a, SandboxCommand>> {
        match egress {
            Some(egress) => Ok(Cow::Owned(egress.command(command)?)),
            None => Ok(Cow::Borrowed(command)),
        }
    }

    pub(crate) fn transport(&self) -> Arc<dyn EgressTransport> {
        self.proxy.transport.clone()
    }

    pub(crate) fn command(&self, command: &SandboxCommand) -> Result<SandboxCommand> {
        ensure!(
            self.is_open(),
            "sandbox egress proxy is closed; acquire the sandbox again"
        );
        let mut command = command.clone();
        command.env.extend(self.proxy.environment().clone());
        for key in [
            "SSL_CERT_FILE",
            "REQUESTS_CA_BUNDLE",
            "NODE_EXTRA_CA_CERTS",
            "CURL_CA_BUNDLE",
            "GIT_SSL_CAINFO",
        ] {
            command.env.insert(key.into(), self.ca_path.clone());
        }
        Ok(command)
    }

    pub(crate) fn close(&self) {
        self.proxy.close();
    }

    fn is_open(&self) -> bool {
        !self.proxy.cancel.is_cancelled() && !self.proxy.transport.is_closed()
    }
}

struct CachedSandbox<H> {
    request: SandboxRequest,
    handle: Option<Arc<H>>,
    egress: Arc<SandboxEgress>,
}

// Shared acquisition logic for the native Firecracker and Lima backends.
// This owns cached handles/proxies, not VM execution or the secret store.
// A hosted backend can own its own allocation lifecycle instead of using this.
pub(crate) struct EgressRuntime<H> {
    resolver: Option<Arc<dyn EgressCredentialResolver>>,
    upstream: Arc<dyn UpstreamResolver>,
    locks: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    sandboxes: Mutex<HashMap<String, CachedSandbox<H>>>,
    closed: CancellationToken,
}

impl<H: ManagedSandboxHandle + 'static> EgressRuntime<H> {
    pub(crate) fn new(
        resolver: Option<Arc<dyn EgressCredentialResolver>>,
        upstream: Arc<dyn UpstreamResolver>,
    ) -> Self {
        Self {
            resolver,
            upstream,
            locks: Mutex::new(HashMap::new()),
            sandboxes: Mutex::new(HashMap::new()),
            closed: CancellationToken::new(),
        }
    }

    fn sandboxes(&self) -> MutexGuard<'_, HashMap<String, CachedSandbox<H>>> {
        self.sandboxes.lock().expect("egress sandbox map poisoned")
    }

    fn ensure_open(&self) -> Result<()> {
        ensure!(
            !self.closed.is_cancelled(),
            "sandbox egress runtime is shut down"
        );
        Ok(())
    }

    async fn lock(&self, id: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().expect("egress lock map poisoned");
            locks.retain(|_, lock| lock.strong_count() > 0);
            match locks.get(id).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(AsyncMutex::new(()));
                    locks.insert(id.to_owned(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        lock.lock_owned().await
    }

    // Called by each backend's ManagedSandboxBackend::acquire implementation.
    // `transport` opens listeners locally or through Lima; `build` acquires the
    // VM, binds its source address, and installs trust. T/B are those closures,
    // and TF/BF are the futures they return. Keeping that work in callbacks lets
    // both backends share cache checks and cleanup without a second VM wrapper.
    pub(crate) async fn acquire<T, TF, B, BF>(
        &self,
        request: SandboxRequest,
        transport: T,
        build: B,
        terminate: impl Future<Output = Result<()>>,
    ) -> Result<Arc<H>>
    where
        T: FnOnce(Vec<String>) -> TF,
        TF: Future<Output = Result<Arc<dyn EgressTransport>>>,
        B: FnOnce(Option<Arc<SandboxEgress>>) -> BF,
        BF: Future<Output = Result<H>>,
    {
        // Serialize this sandbox's acquire/terminate operations; other sandboxes
        // can acquire independently.
        let _guard = self.lock(&request.sandbox_id).await;
        let cached = self.sandboxes().get(&request.sandbox_id).map(|cached| {
            (
                cached.request.spec == request.spec && cached.request.scope == request.scope,
                cached.handle.clone(),
                cached.egress.clone(),
            )
        });
        if let Some((unchanged, handle, egress)) = cached {
            if let Some(handle) = handle
                && egress.is_open()
                && handle.is_running().await? == Some(true)
            {
                ensure!(
                    unchanged,
                    "stop the protected sandbox before changing its configuration"
                );
                return Ok(handle);
            }
            self.remove(&request.sandbox_id).await?;
        }
        // Unrestricted/disabled networking only needs the backend's own rules.
        if !request.spec.policy.requires_proxy() {
            return Ok(Arc::new(build(None).await?));
        }
        self.ensure_open()?;
        ensure!(
            request.lifecycle.idle_ttl.is_some(),
            "proxy egress requires a managed sandbox lifecycle"
        );
        let state = State::new(
            EgressIdentity {
                sandbox_id: request.sandbox_id.clone(),
                scope: request.scope.clone(),
            },
            request.spec.policy.clone(),
            self.resolver.clone(),
            self.upstream.clone(),
        )?;
        // The proxy must exist before boot so the backend can install its
        // endpoints in the VM's network rules. It admits no source yet.
        let transport = transport(state.hosts.iter().cloned().collect()).await?;
        let egress = Arc::new(SandboxEgress {
            proxy: EgressProxy::start_with_transport(transport, state, self.closed.child_token())
                .await?,
            ca_path: format!("/tmp/exo-egress-{}.pem", uuid::Uuid::new_v4().simple()),
        });
        {
            let mut sandboxes = self.sandboxes();
            self.ensure_open()?;
            // Register before boot: shutdown must also close proxies belonging
            // to acquisitions that are still in flight.
            sandboxes.insert(
                request.sandbox_id.clone(),
                CachedSandbox {
                    request: request.clone(),
                    handle: None,
                    egress: egress.clone(),
                },
            );
        }
        let result = async {
            let handle = Arc::new(build(Some(egress.clone())).await?);
            let mut sandboxes = self.sandboxes();
            self.ensure_open()?;
            sandboxes
                .get_mut(&request.sandbox_id)
                .expect("acquiring sandbox remains registered")
                .handle = Some(handle.clone());
            Ok(handle)
        }
        .await;
        if result.is_err() {
            self.sandboxes().remove(&request.sandbox_id);
            egress.close();
            if let Err(cleanup) = egress.proxy.transport.shutdown().await {
                tracing::warn!(sandbox_id = %request.sandbox_id, %cleanup, "egress cleanup after failed acquisition");
            }
            if let Err(cleanup) = terminate.await {
                tracing::warn!(sandbox_id = %request.sandbox_id, %cleanup, "sandbox cleanup after failed acquisition");
            }
        }
        result
    }

    async fn remove(&self, id: &str) -> Result<()> {
        let cached = self.sandboxes().remove(id);
        if let Some(cached) = cached {
            cached.egress.close();
            cached.egress.proxy.transport.shutdown().await?;
        }
        Ok(())
    }

    pub(crate) async fn terminate<F: Future<Output = Result<()>>>(
        &self,
        id: &str,
        terminate: F,
    ) -> Result<()> {
        let _guard = self.lock(id).await;
        self.remove(id).await?;
        terminate.await
    }

    pub(crate) fn shutdown(&self) {
        self.closed.cancel();
        let sandboxes = std::mem::take(&mut *self.sandboxes());
        for cached in sandboxes.into_values() {
            cached.egress.close();
        }
    }
}

impl<H> Drop for EgressRuntime<H> {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::PublicUpstreamResolver;
    use crate::{SandboxLifecycleConfig, SandboxNetworkPolicy, SandboxSpec};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Handle(Option<bool>);

    #[async_trait]
    impl ManagedSandboxHandle for Handle {
        fn id(&self) -> &str {
            "test"
        }
        async fn is_running(&self) -> Result<Option<bool>> {
            Ok(self.0)
        }
        async fn exec(&self, _command: &SandboxCommand) -> Result<crate::SandboxCommandOutput> {
            unreachable!()
        }
        async fn start_process(
            &self,
            _command: &SandboxCommand,
        ) -> Result<crate::SandboxProcessParts> {
            unreachable!()
        }
        async fn stop(&self) -> Result<()> {
            unreachable!()
        }
        async fn detach(&self) -> Result<crate::SandboxAttachment> {
            unreachable!()
        }
        async fn snapshot(&self) -> Result<crate::SnapshotPayload> {
            unreachable!()
        }
    }

    #[derive(Default)]
    struct Transport {
        closed: AtomicBool,
        shutdown_completed: AtomicBool,
    }

    #[async_trait]
    impl EgressTransport for Transport {
        fn endpoints(&self) -> crate::SandboxEgressProxy {
            crate::SandboxEgressProxy {
                http: "192.0.2.1:80".parse().unwrap(),
                https: "192.0.2.1:443".parse().unwrap(),
                dns: "192.0.2.1:53".parse().unwrap(),
            }
        }
        async fn bind_source(&self, _source: Ipv4Addr) -> Result<()> {
            Ok(())
        }
        async fn accept(&self, _tls: bool) -> Result<crate::BoxSandboxTcpStream> {
            std::future::pending().await
        }
        async fn shutdown(&self) -> Result<()> {
            self.close();
            tokio::task::yield_now().await;
            self.shutdown_completed.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
        fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    fn request(id: &str) -> SandboxRequest {
        SandboxRequest {
            sandbox_id: id.into(),
            scope: None,
            provider_state: None,
            spec: SandboxSpec {
                image: "test".into(),
                resources: Default::default(),
                mounts: vec![],
                durable_file_systems: vec![],
                default_workdir: "/tmp".into(),
                policy: SandboxNetworkPolicy::Limited {
                    allowed_hosts: vec!["api.test".into()],
                }
                .into(),
            },
            lifecycle: SandboxLifecycleConfig {
                idle_ttl: Some(Duration::from_secs(60)),
            },
        }
    }

    #[tokio::test]
    async fn failed_acquisition_terminates_the_vm_and_preserves_the_original_error() -> Result<()> {
        for fail_termination in [false, true] {
            let runtime = EgressRuntime::<Handle>::new(None, Arc::new(PublicUpstreamResolver));
            let transport = Arc::new(Transport::default());
            let running = AtomicBool::new(false);
            let result = runtime
                .acquire(
                    request("one"),
                    |_| async { Ok(transport.clone() as Arc<dyn EgressTransport>) },
                    |_| async {
                        running.store(true, Ordering::SeqCst);
                        anyhow::bail!("TLS trust preparation failed")
                    },
                    async {
                        assert!(transport.shutdown_completed.load(Ordering::SeqCst));
                        assert!(runtime.sandboxes().is_empty());
                        assert!(running.swap(false, Ordering::SeqCst));
                        tokio::task::yield_now().await;
                        ensure!(!fail_termination, "VM cleanup failed");
                        Ok(())
                    },
                )
                .await;
            assert_eq!(
                result.err().unwrap().to_string(),
                "TLS trust preparation failed"
            );
            assert!(!running.load(Ordering::SeqCst));
            assert!(transport.is_closed());
            assert!(runtime.sandboxes().is_empty());
            let replacement = Arc::new(Transport::default());
            runtime
                .acquire(
                    request("one"),
                    |_| async { Ok(replacement.clone() as Arc<dyn EgressTransport>) },
                    |_| async { Ok(Handle(Some(true))) },
                    async { panic!("successful acquisition must not terminate the sandbox") },
                )
                .await?;
            assert!(!replacement.is_closed());
            runtime.shutdown();
        }
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_closes_proxies_while_acquisition_is_pending() -> Result<()> {
        let runtime = EgressRuntime::<Handle>::new(None, Arc::new(PublicUpstreamResolver));
        let transport = Arc::new(Transport::default());
        let terminated = AtomicBool::new(false);
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let acquiring = runtime.acquire(
            request("one"),
            |_| async { Ok(transport.clone() as Arc<dyn EgressTransport>) },
            |_| async {
                started.send(()).unwrap();
                released.await?;
                Ok(Handle(Some(true)))
            },
            async {
                assert!(transport.shutdown_completed.load(Ordering::SeqCst));
                terminated.store(true, Ordering::SeqCst);
                Ok(())
            },
        );
        let shutdown = async {
            ready.await.unwrap();
            runtime.shutdown();
            assert!(transport.closed.load(Ordering::SeqCst));
            assert!(runtime.sandboxes.lock().unwrap().is_empty());
            release.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(acquiring, shutdown);
        assert!(result.err().unwrap().to_string().contains("shut down"));
        assert!(terminated.load(Ordering::SeqCst));
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_during_transport_creation_prevents_late_registration() -> Result<()> {
        let runtime = EgressRuntime::<Handle>::new(None, Arc::new(PublicUpstreamResolver));
        let transport = Arc::new(Transport::default());
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let acquiring = runtime.acquire(
            request("one"),
            |_| async {
                started.send(()).unwrap();
                released.await?;
                Ok(transport.clone() as Arc<dyn EgressTransport>)
            },
            |_| async { panic!("closed runtime must not start a sandbox") },
            async { panic!("no sandbox was allocated") },
        );
        let shutdown = async {
            ready.await.unwrap();
            runtime.shutdown();
            release.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(acquiring, shutdown);
        assert!(result.err().unwrap().to_string().contains("shut down"));
        assert!(transport.closed.load(Ordering::SeqCst));
        assert!(runtime.sandboxes.lock().unwrap().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn termination_waits_for_acquisition_and_removes_cached_state() -> Result<()> {
        let runtime = EgressRuntime::<Handle>::new(None, Arc::new(PublicUpstreamResolver));
        let transport = Arc::new(Transport::default());
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let acquiring = runtime.acquire(
            request("one"),
            |_| async { Ok(transport.clone() as Arc<dyn EgressTransport>) },
            |_| async {
                started.send(()).unwrap();
                released.await?;
                Ok(Handle(Some(true)))
            },
            async { panic!("successful acquisition must not terminate the sandbox") },
        );
        let terminating = async {
            ready.await?;
            let terminate = runtime.terminate("one", async { Ok(()) });
            tokio::pin!(terminate);
            assert!(futures::poll!(&mut terminate).is_pending());
            assert!(!transport.closed.load(Ordering::SeqCst));
            release.send(()).unwrap();
            terminate.await
        };
        let (handle, terminated) = tokio::join!(acquiring, terminating);
        handle?;
        terminated?;
        assert!(transport.closed.load(Ordering::SeqCst));
        assert!(runtime.sandboxes.lock().unwrap().is_empty());
        for i in 0..20 {
            runtime.terminate(&i.to_string(), async { Ok(()) }).await?;
        }
        assert_eq!(runtime.locks.lock().unwrap().len(), 1);
        Ok(())
    }
    #[tokio::test]
    async fn dead_or_unknown_handle_does_not_prevent_configuration_changes() -> Result<()> {
        for running in [Some(false), None] {
            let runtime = EgressRuntime::<Handle>::new(None, Arc::new(PublicUpstreamResolver));
            let old = Arc::new(Transport::default());
            let retained = runtime
                .acquire(
                    request("one"),
                    |_| async { Ok(old.clone() as Arc<dyn EgressTransport>) },
                    |_| async { Ok(Handle(running)) },
                    async { panic!("successful acquisition must not terminate the sandbox") },
                )
                .await?;
            let mut changed = request("one");
            changed.spec.image = "new-image".into();
            let new = Arc::new(Transport::default());
            let replacement = runtime
                .acquire(
                    changed,
                    |_| async {
                        assert!(old.shutdown_completed.load(Ordering::SeqCst));
                        Ok(new.clone() as Arc<dyn EgressTransport>)
                    },
                    |_| async { Ok(Handle(Some(true))) },
                    async { panic!("successful acquisition must not terminate the sandbox") },
                )
                .await?;
            assert!(!Arc::ptr_eq(&retained, &replacement));
            assert!(old.is_closed());
            assert!(!new.is_closed());
            runtime.shutdown();
        }
        Ok(())
    }
}
