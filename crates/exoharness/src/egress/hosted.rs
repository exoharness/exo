use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::{Error, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::{
    EgressCredentialResolver, EgressIdentity, EgressProxy, EgressProxyConfig, EgressTransport,
    PublicUpstreamResolver,
};
use crate::{EgressPolicy, SandboxEgressProxy};

/// The allocation that a hosted provider must bind to a reserved relay.
/// Providers must validate both fields before admitting any sandbox traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressAllocation {
    pub allocation_id: String,
    pub generation: u64,
}

/// Values a hosted provider installs in the guest after creating a session.
/// The environment values are placeholders; real credentials stay in the
/// resolver and are substituted only by Exo's proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressGuestConfig {
    pub endpoints: SandboxEgressProxy,
    pub environment: HashMap<String, String>,
    pub ca_pem: String,
}

/// A provider-owned relay that supplies the existing Exo egress transport.
///
/// `egress_transport` must not admit HTTP, HTTPS, or DNS traffic until `attach`
/// succeeds. The hosted session adds a second gate around HTTP/HTTPS accepts;
/// the provider remains authoritative for its transport-owned DNS listener.
#[async_trait]
pub trait HostedEgressTransportSession: Send + Sync {
    fn egress_transport(&self) -> Arc<dyn EgressTransport>;

    /// Verify that the allocation belongs to the identity passed to
    /// `HostedEgressTransport::reserve` and reject every other allocation or
    /// generation. The provider must also keep its DNS path gated by this
    /// attachment.
    async fn attach(&self, allocation: &EgressAllocation) -> Result<()>;

    /// Stop admission, including provider-owned DNS, synchronously, and
    /// cancel/release the provider lease. This is also the cleanup path used
    /// by `Drop`; `shutdown` must wait for listener resources to be released.
    fn close(&self);
    async fn shutdown(&self) -> Result<()>;
}

/// Reserves a provider relay for one Exo sandbox identity before VM boot.
/// The provider must route the guest's isolated network through the returned
/// transport; installing proxy environment variables alone is not sufficient.
/// The reservation and every accepted stream must remain scoped to `identity`.
#[async_trait]
pub trait HostedEgressTransport: Send + Sync {
    /// `allowed_hosts` is canonicalized by Exo and is also the allowlist the
    /// provider should use for its transport-owned DNS listener.
    async fn reserve(
        &self,
        identity: &EgressIdentity,
        allowed_hosts: &[String],
    ) -> Result<Box<dyn HostedEgressTransportSession>>;
}

const SESSION_RESERVED: u8 = 0;
const SESSION_ATTACHING: u8 = 1;
const SESSION_ATTACHED: u8 = 2;
const SESSION_CLOSED: u8 = 3;

struct HostedTransportGate {
    inner: Arc<dyn EgressTransport>,
    state: Arc<AtomicU8>,
    changed: Arc<Notify>,
}

impl HostedTransportGate {
    async fn wait_until_attached(&self) -> Result<()> {
        loop {
            let notified = self.changed.notified();
            match self.state.load(Ordering::Acquire) {
                SESSION_ATTACHED => return Ok(()),
                SESSION_CLOSED => bail!("egress session is closed"),
                _ => notified.await,
            }
        }
    }
}

#[async_trait]
impl EgressTransport for HostedTransportGate {
    fn endpoints(&self) -> SandboxEgressProxy {
        self.inner.endpoints()
    }

    async fn bind_source(&self, source_ip: std::net::Ipv4Addr) -> Result<()> {
        self.wait_until_attached().await?;
        self.inner.bind_source(source_ip).await
    }

    async fn accept(&self, tls: bool) -> Result<crate::BoxSandboxTcpStream> {
        self.wait_until_attached().await?;
        let stream = self.inner.accept(tls).await?;
        ensure!(
            self.state.load(Ordering::Acquire) == SESSION_ATTACHED,
            "egress session is closed"
        );
        Ok(stream)
    }

    fn is_closed(&self) -> bool {
        self.state.load(Ordering::Acquire) == SESSION_CLOSED || self.inner.is_closed()
    }

    fn close(&self) {
        self.state.store(SESSION_CLOSED, Ordering::Release);
        self.changed.notify_waiters();
        self.inner.close();
    }
}

/// Owns one hosted relay and the same policy-enforcing proxy used by native
/// Firecracker/Lima sandboxes.
pub struct HostedEgressSession {
    relay: Box<dyn HostedEgressTransportSession>,
    proxy: EgressProxy,
    state: Arc<AtomicU8>,
    changed: Arc<Notify>,
}

impl HostedEgressSession {
    /// Reserve the provider relay and start Exo's policy-enforcing proxy.
    /// The returned session remains unable to admit traffic until `attach`.
    pub async fn start(
        transport: Arc<dyn HostedEgressTransport>,
        identity: EgressIdentity,
        policy: EgressPolicy,
        resolver: Option<Arc<dyn EgressCredentialResolver>>,
    ) -> Result<Self> {
        let proxy_config = EgressProxyConfig::new(
            identity.clone(),
            policy,
            resolver,
            Arc::new(PublicUpstreamResolver),
        )?;
        let allowed_hosts = proxy_config.allowed_hosts();
        let relay = transport.reserve(&identity, &allowed_hosts).await?;
        let session_state = Arc::new(AtomicU8::new(SESSION_RESERVED));
        let changed = Arc::new(Notify::new());
        let gated_transport = Arc::new(HostedTransportGate {
            inner: relay.egress_transport(),
            state: session_state.clone(),
            changed: changed.clone(),
        });
        let proxy = match proxy_config
            .start(gated_transport, CancellationToken::new())
            .await
        {
            Ok(proxy) => proxy,
            Err(error) => return Err(cleanup_hosted_relay(relay.as_ref(), error).await),
        };
        Ok(Self {
            relay,
            proxy,
            state: session_state,
            changed,
        })
    }

    /// Return the guest-facing proxy endpoints, credential placeholders, and
    /// CA certificate that the provider should install in the sandbox. Exo
    /// cannot install hosted guest network rules itself, so the provider must
    /// route all guest traffic through the reserved transport as part of its
    /// network setup.
    pub fn guest_config(&self) -> EgressGuestConfig {
        EgressGuestConfig {
            endpoints: self.proxy.endpoints(),
            environment: self.proxy.environment().clone(),
            ca_pem: self.proxy.ca_pem().to_owned(),
        }
    }

    /// Bind the reserved relay to the exact provider allocation and generation.
    /// A failed attachment permanently closes the session.
    pub async fn attach(&self, allocation: &EgressAllocation) -> Result<()> {
        ensure!(
            !allocation.allocation_id.is_empty(),
            "egress allocation is required"
        );
        match self.state.compare_exchange(
            SESSION_RESERVED,
            SESSION_ATTACHING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(SESSION_ATTACHED) => bail!("egress allocation is already attached"),
            Err(SESSION_CLOSED) => bail!("egress session is closed"),
            Err(_) => bail!("egress allocation attachment is already in progress"),
        }
        let mut cleanup = AttachCleanup {
            relay: self.relay.as_ref(),
            state: self.state.as_ref(),
            changed: self.changed.as_ref(),
            armed: true,
        };
        if let Err(error) = self.relay.attach(allocation).await {
            drop(cleanup);
            return Err(cleanup_hosted_relay(self.relay.as_ref(), error).await);
        }
        if self
            .state
            .compare_exchange(
                SESSION_ATTACHING,
                SESSION_ATTACHED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            drop(cleanup);
            return Err(cleanup_hosted_relay(
                self.relay.as_ref(),
                anyhow!("egress session closed during attachment"),
            )
            .await);
        }
        self.changed.notify_waiters();
        cleanup.armed = false;
        Ok(())
    }

    pub fn close(&self) {
        self.proxy.close();
        self.relay.close();
    }

    /// Close the Exo proxy and wait for both proxy and provider resources to
    /// finish releasing.
    pub async fn shutdown(mut self) -> Result<()> {
        self.close();
        let (proxy_result, relay_result) =
            tokio::join!(self.proxy.join_with_timeout(), self.relay.shutdown());
        match (proxy_result, relay_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(proxy), Err(relay)) => Err(anyhow!(
                "hosted egress proxy cleanup failed: {proxy}; relay cleanup failed: {relay}"
            )),
        }
    }
}

struct AttachCleanup<'a> {
    relay: &'a dyn HostedEgressTransportSession,
    state: &'a AtomicU8,
    changed: &'a Notify,
    armed: bool,
}

impl Drop for AttachCleanup<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.state.store(SESSION_CLOSED, Ordering::Release);
            self.changed.notify_waiters();
            self.relay.close();
        }
    }
}

async fn cleanup_hosted_relay(relay: &dyn HostedEgressTransportSession, error: Error) -> Error {
    relay.close();
    match relay.shutdown().await {
        Ok(()) => error,
        Err(cleanup) => anyhow!("{error}; hosted egress cleanup failed: {cleanup}"),
    }
}

impl Drop for HostedEgressSession {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{
        BoxSandboxTcpStream, CredentialInjectionLocation, CredentialNetworkPolicy,
        EgressCredentialBinding, SandboxNetworkPolicy,
    };

    const ENDPOINTS: SandboxEgressProxy = SandboxEgressProxy {
        http: SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 18080),
        https: SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 18443),
        dns: SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 18553),
    };

    #[derive(Default)]
    struct Events {
        identities: Vec<String>,
        allowed_hosts: Vec<Vec<String>>,
        allocations: Vec<EgressAllocation>,
        closed: bool,
        shutdown: bool,
    }

    struct FakeProvider {
        events: Arc<Mutex<Events>>,
        endpoints: SandboxEgressProxy,
        accepts: Arc<AtomicUsize>,
        attach_error: bool,
    }

    struct FakeRelay {
        events: Arc<Mutex<Events>>,
        transport: Arc<FakeEgressTransport>,
        attach_error: bool,
    }

    struct FakeEgressTransport {
        endpoints: SandboxEgressProxy,
        accepts: Arc<AtomicUsize>,
        closed: CancellationToken,
    }

    #[async_trait]
    impl HostedEgressTransport for FakeProvider {
        async fn reserve(
            &self,
            identity: &EgressIdentity,
            allowed_hosts: &[String],
        ) -> Result<Box<dyn HostedEgressTransportSession>> {
            let mut events = self.events.lock().expect("events lock");
            events.identities.push(identity.sandbox_id.clone());
            events.allowed_hosts.push(allowed_hosts.to_vec());
            Ok(Box::new(FakeRelay {
                events: self.events.clone(),
                transport: Arc::new(FakeEgressTransport {
                    endpoints: self.endpoints,
                    accepts: self.accepts.clone(),
                    closed: CancellationToken::new(),
                }),
                attach_error: self.attach_error,
            }))
        }
    }

    #[async_trait]
    impl HostedEgressTransportSession for FakeRelay {
        fn egress_transport(&self) -> Arc<dyn EgressTransport> {
            self.transport.clone()
        }

        async fn attach(&self, allocation: &EgressAllocation) -> Result<()> {
            self.events
                .lock()
                .expect("events lock")
                .allocations
                .push(allocation.clone());
            if self.attach_error {
                bail!("attachment rejected")
            }
            Ok(())
        }

        fn close(&self) {
            self.events.lock().expect("events lock").closed = true;
            self.transport.close();
        }

        async fn shutdown(&self) -> Result<()> {
            self.events.lock().expect("events lock").shutdown = true;
            self.transport.close();
            Ok(())
        }
    }

    #[async_trait]
    impl EgressTransport for FakeEgressTransport {
        fn endpoints(&self) -> SandboxEgressProxy {
            self.endpoints
        }

        async fn bind_source(&self, _source_ip: Ipv4Addr) -> Result<()> {
            Ok(())
        }

        async fn accept(&self, _tls: bool) -> Result<BoxSandboxTcpStream> {
            self.accepts.fetch_add(1, Ordering::Relaxed);
            self.closed.cancelled().await;
            bail!("fake egress transport closed")
        }

        fn is_closed(&self) -> bool {
            self.closed.is_cancelled()
        }

        fn close(&self) {
            self.closed.cancel();
        }
    }

    struct TestResolver;

    #[async_trait]
    impl EgressCredentialResolver for TestResolver {
        async fn resolve(
            &self,
            _identity: &EgressIdentity,
            _binding_name: &str,
            _destination: &super::super::EgressDestination,
        ) -> Result<String> {
            bail!("unused in hosted session test")
        }
    }

    fn provider_with(
        attach_error: bool,
        endpoints: SandboxEgressProxy,
    ) -> (
        Arc<dyn HostedEgressTransport>,
        Arc<Mutex<Events>>,
        Arc<AtomicUsize>,
    ) {
        let events = Arc::new(Mutex::new(Events::default()));
        let accepts = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(FakeProvider {
            events: events.clone(),
            endpoints,
            accepts: accepts.clone(),
            attach_error,
        });
        (provider, events, accepts)
    }

    fn identity() -> EgressIdentity {
        EgressIdentity {
            sandbox_id: "sandbox-1".into(),
            scope: None,
        }
    }

    fn policy() -> EgressPolicy {
        EgressPolicy {
            networking: SandboxNetworkPolicy::Limited {
                allowed_hosts: vec!["github.com".into()],
            },
            credentials: vec![EgressCredentialBinding {
                name: "github-read-token".into(),
                environment_variable: "EXO_GITHUB_TOKEN".into(),
                networking: CredentialNetworkPolicy::Limited {
                    allowed_hosts: vec!["github.com".into()],
                },
                injection_location: CredentialInjectionLocation { header: true },
            }],
        }
    }

    #[tokio::test]
    async fn hosted_session_uses_proxy_and_binds_allocation() -> Result<()> {
        let (provider, events, accepts) = provider_with(false, ENDPOINTS);
        let session = HostedEgressSession::start(
            provider,
            identity(),
            policy(),
            Some(Arc::new(TestResolver)),
        )
        .await?;
        let config = session.guest_config();
        assert_eq!(config.endpoints, ENDPOINTS);
        assert!(config.ca_pem.contains("BEGIN CERTIFICATE"));
        assert!(config.environment["EXO_GITHUB_TOKEN"].starts_with("exo_egress_"));
        tokio::task::yield_now().await;
        assert_eq!(accepts.load(Ordering::Relaxed), 0);

        let allocation = EgressAllocation {
            allocation_id: "allocation-1".into(),
            generation: 7,
        };
        session.attach(&allocation).await?;
        tokio::task::yield_now().await;
        assert!(accepts.load(Ordering::Relaxed) > 0);

        session.shutdown().await?;
        let events = events.lock().expect("events lock");
        assert_eq!(events.identities, ["sandbox-1"]);
        assert_eq!(events.allowed_hosts, [["github.com".to_string()]]);
        assert_eq!(events.allocations, [allocation]);
        assert!(events.closed);
        assert!(events.shutdown);
        Ok(())
    }

    #[tokio::test]
    async fn failed_attachment_is_terminal_and_closes_relay() -> Result<()> {
        let (provider, events, _) = provider_with(true, ENDPOINTS);
        let session = HostedEgressSession::start(
            provider,
            identity(),
            policy(),
            Some(Arc::new(TestResolver)),
        )
        .await?;
        let allocation = EgressAllocation {
            allocation_id: "allocation-1".into(),
            generation: 1,
        };
        assert!(session.attach(&allocation).await.is_err());
        assert!(session.attach(&allocation).await.is_err());
        assert!(events.lock().expect("events lock").closed);
        session.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn invalid_policy_does_not_reserve_relay() -> Result<()> {
        let (provider, events, _) = provider_with(false, ENDPOINTS);
        let result = HostedEgressSession::start(
            provider,
            identity(),
            EgressPolicy::from(SandboxNetworkPolicy::Unrestricted),
            None,
        )
        .await;
        assert!(result.is_err());
        let events = events.lock().expect("events lock");
        assert!(events.identities.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn proxy_start_failure_releases_reserved_relay() -> Result<()> {
        let endpoints = SandboxEgressProxy {
            http: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 18080),
            ..ENDPOINTS
        };
        let (provider, events, _) = provider_with(false, endpoints);
        let result = HostedEgressSession::start(
            provider,
            identity(),
            policy(),
            Some(Arc::new(TestResolver)),
        )
        .await;
        assert!(result.is_err());
        let events = events.lock().expect("events lock");
        assert!(events.closed);
        assert!(events.shutdown);
        Ok(())
    }
}
