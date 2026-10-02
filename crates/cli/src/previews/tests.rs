use super::*;
use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, ExoHarness, NewAgentRequest,
    SandboxBackendRegistration, SandboxProvider, SecretBackendChoice,
};

#[test]
fn hostnames_are_stable_valid_and_distinguish_threads_with_the_same_name() -> Result<()> {
    let hostname = thread_hostname("Braintrust Dev", "Hello / world", "first", "exo.localhost")?;
    assert_eq!(
        hostname,
        thread_hostname("Braintrust Dev", "Hello / world", "first", "exo.localhost")?
    );
    assert_ne!(
        hostname,
        thread_hostname("Braintrust Dev", "Hello / world", "second", "exo.localhost")?
    );
    assert!(hostname.starts_with("hello-world-"));
    assert!(hostname.ends_with(".braintrust-dev.exo.localhost"));
    let long = thread_hostname(&"a".repeat(128), &"b".repeat(128), "id", "exo.localhost")?;
    assert!(long.split('.').all(|label| label.len() <= 63));
    Ok(())
}

#[tokio::test]
async fn listener_reuses_its_port_and_reports_a_conflict_without_changing_the_url() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("listener.json");
    let first = saved_listener(&path).await?;
    let port = first.local_addr()?.port();
    let saved = std::fs::read(&path)?;
    assert!(saved_listener(&path).await.is_err());
    assert_eq!(std::fs::read(&path)?, saved);
    drop(first);
    let resumed = saved_listener(&path).await?;
    assert_eq!(resumed.local_addr()?.port(), port);
    Ok(())
}

#[tokio::test]
async fn request_routing_keeps_upgrade_headers_and_bytes_after_the_headers() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let mut client = TcpStream::connect(listener.local_addr()?).await?;
    let (mut server, _) = listener.accept().await?;
    let request = b"GET /realtime?session=1 HTTP/1.1\r\nHost: 13000.Thread.Exo.Localhost:1234\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\nfirst-frame";
    client.write_all(request).await?;
    let (host, bytes) = read_request(&mut server).await?;
    assert_eq!(host, "13000.thread.exo.localhost:1234");
    assert_eq!(bytes, request);
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
async fn portal_unknown_hosts_and_stopped_services_have_distinct_responses() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let harness = BasicExoHarness::new(BasicExoHarnessConfig {
        root: temp.path().to_owned(),
        secret_backend: SecretBackendChoice::Static([7; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_policy: None,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
    })
    .await?;
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".into(),
            name: "Agent".into(),
            vaults: vec![],
        })
        .await?;
    let thread = agent.new_thread(Default::default()).await?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let portal = format!("thread.exo.localhost:{}", address.port());
    let service = format!("13000.{portal}");
    let previews = vec![BrowserPreview {
        name: "app".into(),
        port: 13000,
        url: format!("http://{service}"),
    }];
    let routes = BTreeMap::from([(service.clone(), 13000)]);
    let task = tokio::spawn(serve(listener, thread, portal.clone(), previews, routes));
    let http = reqwest::Client::new();
    let portal_response = http
        .get(format!("http://{address}"))
        .header("Host", &portal)
        .send()
        .await?;
    assert_eq!(portal_response.status(), 200);
    assert!(
        portal_response
            .text()
            .await?
            .contains(&format!("http://{service}"))
    );
    assert_eq!(
        http.get(format!("http://{address}"))
            .header("Host", "unknown.localhost")
            .send()
            .await?
            .status(),
        404
    );
    let stopped = http
        .get(format!("http://{address}"))
        .header("Host", service)
        .send()
        .await?;
    assert_eq!(stopped.status(), 502);
    assert!(stopped.text().await?.contains("Resume the thread"));
    task.abort();
    Ok(())
}
