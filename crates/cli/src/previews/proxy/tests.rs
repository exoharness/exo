use super::*;
use tokio::task::JoinHandle;

async fn client(directory: &Path) -> Result<Client> {
    timeout(Duration::from_secs(2), async {
        loop {
            if let Some(mut stream) = existing_proxy(directory).await? {
                let Response::Listening { port } = receive(&mut stream).await? else {
                    bail!("unexpected greeting");
                };
                return Ok::<_, anyhow::Error>(Client { stream, port });
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

fn registration(gateway: PathBuf, name: &str, port: u16) -> Registration {
    let portal = format!("{name}.exo.localhost:{port}");
    Registration {
        gateway,
        portal: portal.clone(),
        services: [("app", 5173), ("api", 8000)]
            .into_iter()
            .map(|(name, guest)| Service {
                name: name.into(),
                port: guest,
                host: format!("{name}.{portal}"),
            })
            .collect(),
    }
}

fn gateway(listener: UnixListener, name: &'static str) -> JoinHandle<Result<()>> {
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let port = stream.read_u16().await?;
            stream.write_u8(1).await?;
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            while !bytes.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).await?;
                ensure!(count > 0, "request ended before headers");
                bytes.extend_from_slice(&chunk[..count]);
            }
            let body = format!("{name}:{port}");
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
        }
    })
}

async fn request(port: u16, host: &str) -> Result<reqwest::Response> {
    Ok(reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/api/example"))
        .header("Host", host)
        .send()
        .await?)
}

#[tokio::test]
async fn sessions_share_one_port_and_removing_one_keeps_the_other_running() -> Result<()> {
    let state = tempfile::tempdir()?;
    let directory = state.path().to_owned();
    let proxy = tokio::spawn(async move { run(&directory).await });
    let mut first = client(state.path()).await?;
    let mut second = client(state.path()).await?;
    assert_eq!(first.port, second.port);
    let port = first.port;
    let sockets = tempfile::tempdir_in("/tmp")?;
    let first_socket = sockets.path().join("first.sock");
    let second_socket = sockets.path().join("second.sock");
    let first_gateway = gateway(UnixListener::bind(&first_socket)?, "first");
    let second_gateway = gateway(UnixListener::bind(&second_socket)?, "second");
    first
        .register(registration(first_socket.clone(), "first", port))
        .await?;
    second
        .register(registration(second_socket, "second", port))
        .await?;
    for (host, expected) in [
        ("app.first", "first:5173"),
        ("api.first", "first:8000"),
        ("app.second", "second:5173"),
        ("api.second", "second:8000"),
    ] {
        assert_eq!(
            request(port, &format!("{host}.exo.localhost:{port}"))
                .await?
                .text()
                .await?,
            expected
        );
    }
    let portal = request(port, &format!("first.exo.localhost:{port}")).await?;
    assert_eq!(portal.status(), 200);
    assert!(
        portal
            .text()
            .await?
            .contains(&format!("http://api.first.exo.localhost:{port}"))
    );
    assert_eq!(request(port, "unknown.exo.localhost").await?.status(), 404);

    drop(first);
    timeout(Duration::from_secs(2), async {
        while request(port, &format!("app.first.exo.localhost:{port}"))
            .await?
            .status()
            != 404
        {
            sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert_eq!(
        request(port, &format!("api.second.exo.localhost:{port}"))
            .await?
            .text()
            .await?,
        "second:8000"
    );
    let mut resumed = client(state.path()).await?;
    assert_eq!(resumed.port, port);
    resumed
        .register(registration(first_socket, "first", port))
        .await?;
    assert_eq!(
        request(port, &format!("app.first.exo.localhost:{port}"))
            .await?
            .text()
            .await?,
        "first:5173"
    );
    drop(resumed);
    drop(second);
    timeout(Duration::from_secs(7), proxy).await???;
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    first_gateway.abort();
    second_gateway.abort();
    Ok(())
}

#[tokio::test]
async fn unavailable_services_return_502_and_upgrade_bytes_are_preserved() -> Result<()> {
    let state = tempfile::tempdir()?;
    let directory = state.path().to_owned();
    let proxy = tokio::spawn(async move { run(&directory).await });
    let mut session = client(state.path()).await?;
    let port = session.port;
    let sockets = tempfile::tempdir_in("/tmp")?;
    let socket = sockets.path().join("gateway.sock");
    session
        .register(registration(socket.clone(), "demo", port))
        .await?;
    let host = format!("app.demo.exo.localhost:{port}");
    let unavailable = request(port, &host).await?;
    assert_eq!(unavailable.status(), 502);
    assert!(unavailable.text().await?.contains("Resume the thread"));

    let gateway = UnixListener::bind(socket)?;
    let upgrade = format!(
        "GET /socket HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\nfirst-frame"
    );
    let expected = upgrade.clone();
    let backend = tokio::spawn(async move {
        let (mut stream, _) = gateway.accept().await?;
        assert_eq!(stream.read_u16().await?, 5173);
        stream.write_u8(1).await?;
        let mut bytes = vec![0; expected.len()];
        stream.read_exact(&mut bytes).await?;
        assert_eq!(bytes, expected.as_bytes());
        stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\nreply-frame").await?;
        let mut next = [0; 10];
        stream.read_exact(&mut next).await?;
        assert_eq!(&next, b"next-frame");
        Ok::<_, anyhow::Error>(())
    });
    let mut browser = TcpStream::connect(("127.0.0.1", port)).await?;
    browser.write_all(upgrade.as_bytes()).await?;
    let expected_reply = b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\nreply-frame";
    let mut reply = vec![0; expected_reply.len()];
    browser.read_exact(&mut reply).await?;
    assert_eq!(reply, expected_reply);
    browser.write_all(b"next-frame").await?;
    timeout(Duration::from_secs(2), backend).await???;
    drop(session);
    proxy.abort();
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
