use super::*;
use exoharness::{ManagedSandboxBackend, ManagedSandboxHandle, SandboxProvider, SandboxRequest};

struct TcpBackend;

#[async_trait::async_trait]
impl ManagedSandboxBackend for TcpBackend {
    fn is_local(&self) -> bool {
        true
    }
    fn consumable_snapshot_formats(&self) -> &[exoharness::SnapshotFormat] {
        &[]
    }
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        exoharness::LocalProcessSandboxBackend
            .acquire(request)
            .await
    }
    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: exoharness::SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        anyhow::bail!("unsupported")
    }
    async fn connect_tcp(
        &self,
        request: SandboxRequest,
        port: u16,
    ) -> Result<Option<exoharness::BoxSandboxTcpStream>> {
        ensure!(request.spec.tcp_ports.contains(&port), "unpublished port");
        Ok(Some(Box::pin(
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await?,
        )))
    }
    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: exoharness::SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        anyhow::bail!("unsupported")
    }
}

#[tokio::test]
async fn direct_connections_preserve_upgrade_bytes_and_report_unavailable_services() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let backend = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let guest_port = backend.local_addr()?.port();
    let mut config = exoharness::test_support::local_test_config(temp.path());
    config
        .sandbox_backends
        .push(exoharness::SandboxBackendRegistration::from_backend(
            SandboxProvider::Smolvm,
            Arc::new(TcpBackend),
        ));
    let harness = exoharness::BasicExoHarness::new(config).await?;
    let agent = exoharness::test_support::new_test_agent(&harness, "web").await?;
    let mut request = exoharness::test_support::sandbox_request();
    request.provider = SandboxProvider::Smolvm;
    request.tcp_ports = vec![guest_port];
    let environment = exoharness::EnvironmentDefinition {
        name: "dev".into(),
        config: request.clone(),
    };
    let thread = agent
        .new_thread(exoharness::NewThreadRequest {
            environment: Some(environment.clone()),
            slug: Some("demo".into()),
            ..Default::default()
        })
        .await?;
    let proxy = PreviewProxy::start(None, "localhost").await?;
    let previews = previews_for(thread.record(), &proxy.endpoint)?.context("previews")?;
    proxy.register(agent.record().id, thread.clone(), &previews);
    let endpoint = format!("http://127.0.0.1:{}", proxy.endpoint.port);
    let http = reqwest::Client::new();
    assert_eq!(
        http.get(&endpoint)
            .header("Host", "unknown.localhost")
            .send()
            .await?
            .status(),
        404
    );
    let page = http
        .get(&endpoint)
        .header("Host", &previews.host)
        .send()
        .await?;
    assert_eq!(page.status(), 200);
    assert!(page.text().await?.contains(&previews.services[0].url));
    assert_eq!(
        http.get(&endpoint)
            .header("Host", &previews.services[0].host)
            .send()
            .await?
            .status(),
        502
    );
    thread.create_sandbox(request).await?;
    exoharness::test_support::corrupt_thread_history(
        temp.path(),
        agent.record().id,
        thread.record().id,
    )?;
    assert!(thread.get_events(None).await.is_err());

    let upgrade = format!(
        "GET /socket?session=1 HTTP/1.1\r\nHost: {}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\nfirst-frame",
        previews.services[0].host
    );
    let expected = upgrade.clone();
    let response = b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\nreply-frame";
    let worker = tokio::spawn(async move {
        let (mut stream, _) = backend.accept().await?;
        let mut bytes = vec![0; expected.len()];
        stream.read_exact(&mut bytes).await?;
        assert_eq!(bytes, expected.as_bytes());
        stream.write_all(response).await?;
        let mut next = [0; 10];
        stream.read_exact(&mut next).await?;
        assert_eq!(&next, b"next-frame");
        Ok::<_, anyhow::Error>(())
    });
    let mut browser = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.endpoint.port)).await?;
    browser.write_all(upgrade.as_bytes()).await?;
    let mut bytes = vec![0; response.len()];
    timeout(Duration::from_secs(2), browser.read_exact(&mut bytes)).await??;
    assert_eq!(bytes, response);
    browser.write_all(b"next-frame").await?;
    timeout(Duration::from_secs(2), worker).await???;
    proxy.remove(thread.record().id);
    assert_eq!(
        http.get(endpoint)
            .header("Host", &previews.host)
            .send()
            .await?
            .status(),
        404
    );
    Ok(())
}

#[tokio::test]
async fn hostnames_are_stable_bounded_and_distinguish_equal_thread_names() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let harness =
        exoharness::BasicExoHarness::new(exoharness::test_support::local_test_config(temp.path()))
            .await?;
    let agent = exoharness::test_support::new_test_agent(&harness, "web").await?;
    let thread = agent.new_thread(Default::default()).await?;
    let mut record = thread.record().clone();
    record.slug = "Hello / world".repeat(20);
    let mut config = exoharness::test_support::sandbox_request();
    config.tcp_ports = vec![5173, 8000];
    let mut env = exoharness::EnvironmentDefinition {
        name: "dev".into(),
        config,
    };
    let endpoint = PreviewEndpoint {
        domain: "localhost".into(),
        port: 1234,
    };
    record.environment = Some(env.clone());
    let first = previews_for(&record, &endpoint)?.context("previews")?;
    assert_eq!(first, previews_for(&record, &endpoint)?.unwrap());
    assert!(first.host.starts_with("hello---world"));
    assert!(first.host.split('.').all(|label| label.len() <= 63));
    record.id = exoharness::Uuid7::now();
    assert_ne!(first.page, previews_for(&record, &endpoint)?.unwrap().page);
    for slug in ["测试", "---"] {
        record.slug = slug.into();
        let preview = previews_for(&record, &endpoint)?.unwrap();
        assert_eq!(preview.host.split('.').next().unwrap().len(), 8);
    }
    env.config.tcp_ports.clear();
    record.environment = Some(env);
    assert!(previews_for(&record, &endpoint)?.is_none());
    Ok(())
}

#[tokio::test]
async fn duplicate_host_headers_are_rejected() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let mut client = TcpStream::connect(listener.local_addr()?).await?;
    let (mut server, _) = listener.accept().await?;
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: first\r\nHost: second\r\n\r\n")
        .await?;
    assert!(read_request(&mut server).await.is_err());
    Ok(())
}

#[tokio::test]
async fn saved_listener_reuses_its_port_and_replaces_conflicts() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut first = PreviewProxy::start_server(temp.path(), "localhost").await?;
    let port = first.endpoint.port;
    let replacement = PreviewProxy::start_server(temp.path(), "localhost").await?;
    assert_ne!(replacement.endpoint.port, port);
    let replacement_port = replacement.endpoint.port;
    let mut replacement = replacement;
    replacement.stop();
    assert!((&mut replacement.task).await.unwrap_err().is_cancelled());
    let path = temp.path().join("previews/listener.json");
    let saved = std::fs::read(&path)?;
    first.stop();
    assert!((&mut first.task).await.unwrap_err().is_cancelled());
    let resumed = PreviewProxy::start_server(temp.path(), "localhost").await?;
    assert_eq!(resumed.endpoint.port, replacement_port);
    assert_eq!(std::fs::read(path)?, saved);
    Ok(())
}

#[tokio::test]
async fn server_restart_registers_saved_previews_without_opening_threads() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let config = exoharness::test_support::local_test_config(temp.path().join("state"));
    let state = Arc::new(exoharness::BasicExoHarness::new(config.clone()).await?);
    let agent = exoharness::test_support::new_test_agent(state.as_ref(), "web").await?;
    let mut request = exoharness::test_support::sandbox_request();
    request.tcp_ports = vec![8000];
    let thread = agent
        .new_thread(exoharness::NewThreadRequest {
            environment: Some(exoharness::EnvironmentDefinition {
                name: "web".into(),
                config: request,
            }),
            ..Default::default()
        })
        .await?;
    let runtime = || {
        crate::Runtime::new(
            crate::LocalProvider::managed(
                state.clone(),
                config.clone(),
                Default::default(),
                Arc::new(cost::PricingTable::empty()),
            )
            .unwrap(),
            None,
        )
    };
    let first = runtime();
    let endpoint = first
        .start_preview_server(temp.path(), "localhost", Some(agent.record().id))
        .await?;
    let urls = previews_for(thread.record(), &endpoint)?.unwrap();
    first.shutdown().await?;
    timeout(Duration::from_secs(2), async {
        while TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.port))
            .await
            .is_ok()
        {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let restarted = runtime();
    let endpoint = restarted
        .start_preview_server(temp.path(), "localhost", Some(agent.record().id))
        .await?;
    assert_eq!(endpoint.port, url::Url::parse(&urls.page)?.port().unwrap());
    let page = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{}", endpoint.port))
        .header("Host", &urls.host)
        .send()
        .await?;
    assert_eq!(page.status(), 200);
    assert!(page.text().await?.contains(&urls.services[0].url));
    restarted.shutdown().await
}
