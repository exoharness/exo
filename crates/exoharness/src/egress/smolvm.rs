use super::*;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

pub(crate) struct SmolvmProxy {
    connection: ProxyConnection,
    address: SocketAddr,
    token: [u8; 32],
}

impl SmolvmProxy {
    pub(crate) async fn start(state: State, cancel: CancellationToken) -> Result<Self> {
        let listener = Arc::new(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?);
        let address = listener.local_addr()?;
        let mut token = [0; 32];
        token[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        token[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        let connection = ProxyConnection::start(
            state,
            cancel,
            move || {
                let listener = listener.clone();
                async move { Ok(Connection::Smolvm(listener.accept().await?.0, token)) }
            },
            || {},
        )?;
        Ok(Self {
            connection,
            address,
            token,
        })
    }

    pub(crate) fn configure(&self, command: &mut tokio::process::Command) {
        let token: String = self
            .token
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        command
            .arg("--egress-interceptor")
            .arg(self.address.to_string())
            .env("SMOLVM_INTERCEPTOR_TOKEN", token);
    }
}

impl sandbox::SandboxProxy for SmolvmProxy {
    fn connection(&self) -> &ProxyConnection {
        &self.connection
    }
}

pub(super) async fn connection(
    mut stream: TcpStream,
    token: [u8; 32],
    tls: TlsAcceptor,
    state: Arc<State>,
) -> Result<()> {
    let port = tokio::time::timeout(IO_TIMEOUT, async {
        let mut header = [0; 44];
        stream.read_exact(&mut header).await?;
        ensure!(
            &header[..9] == b"SMOLICPT\x01",
            "invalid interceptor protocol"
        );
        let mismatch = header[9..41]
            .iter()
            .zip(token)
            .fold(0, |diff, (a, b)| diff | (a ^ b));
        ensure!(mismatch == 0, "invalid interceptor token");
        let port = u16::from_be_bytes([header[42], header[43]]);
        let mut address = [0; 16];
        let size = match header[41] {
            4 => 4,
            6 => 16,
            _ => anyhow::bail!("invalid interceptor address family"),
        };
        stream.read_exact(&mut address[..size]).await?;
        let allowed = size == 4
            && public_ipv4(IpAddr::V4(Ipv4Addr::new(
                address[0], address[1], address[2], address[3],
            )))
            && matches!(port, 80 | 443);
        stream.write_all(&[if allowed { 0 } else { 13 }]).await?;
        ensure!(allowed, "intercepted destination is not permitted");
        Ok::<_, anyhow::Error>(port)
    })
    .await??;
    if port == 443 {
        transparent_https_connection(stream, tls, state).await
    } else {
        http_connection(stream, state, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::sandbox::SandboxProxy;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Upstream(AtomicUsize);

    #[async_trait]
    impl UpstreamResolver for Upstream {
        async fn resolve(&self, _host: &str, _port: u16) -> Result<ResolvedUpstream> {
            self.0.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("test upstream");
        }
    }

    async fn proxy(upstream: Arc<Upstream>) -> Result<SmolvmProxy> {
        SmolvmProxy::start(
            State::new(
                EgressIdentity {
                    sandbox_id: "test".into(),
                    scope: Default::default(),
                },
                SandboxNetworkPolicy::Limited {
                    allowed_hosts: vec!["api.test".into()],
                }
                .into(),
                None,
                upstream,
            )?,
            CancellationToken::new(),
        )
        .await
    }

    async fn connect(
        proxy: &SmolvmProxy,
        token: [u8; 32],
        destination: SocketAddr,
    ) -> Result<TcpStream> {
        let mut stream = TcpStream::connect(proxy.address).await?;
        let mut header = b"SMOLICPT\x01".to_vec();
        header.extend(token);
        header.push(if destination.is_ipv4() { 4 } else { 6 });
        header.extend(destination.port().to_be_bytes());
        match destination.ip() {
            IpAddr::V4(ip) => header.extend(ip.octets()),
            IpAddr::V6(ip) => header.extend(ip.octets()),
        }
        stream.write_all(&header).await?;
        Ok(stream)
    }

    #[tokio::test]
    async fn interceptor_authenticates_each_sandbox_and_rejects_unsupported_destinations()
    -> Result<()> {
        let upstream = Arc::new(Upstream::default());
        let first = proxy(upstream.clone()).await?;
        let second = proxy(upstream.clone()).await?;
        let public = "93.184.216.34:80".parse()?;
        let mut wrong = connect(&second, first.token, public).await?;
        assert!(wrong.read_u8().await.is_err());
        for destination in [
            "93.184.216.34:22",
            "93.184.216.34:8443",
            "127.0.0.1:443",
            "169.254.169.254:80",
            "[2606:4700::1111]:443",
        ] {
            let mut stream = connect(&first, first.token, destination.parse()?).await?;
            assert_ne!(stream.read_u8().await?, 0, "{destination}");
        }
        for (host, expected_uses) in [("blocked.test", 0), ("api.test", 1)] {
            let mut stream = connect(&first, first.token, public).await?;
            assert_eq!(stream.read_u8().await?, 0);
            stream
                .write_all(
                    format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await?;
            let mut response = String::new();
            stream.read_to_string(&mut response).await?;
            assert!(response.starts_with("HTTP/1.1 502"), "{response}");
            assert_eq!(upstream.0.load(Ordering::SeqCst), expected_uses);
        }
        let mut pending = TcpStream::connect(first.address).await?;
        first.close();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), pending.read_u8())
                .await?
                .is_err()
        );
        second.close();
        Ok(())
    }

    #[tokio::test]
    async fn interceptor_token_is_passed_only_in_the_host_environment() -> Result<()> {
        let proxy = proxy(Arc::new(Upstream::default())).await?;
        let mut command = tokio::process::Command::new("smolvm");
        proxy.configure(&mut command);
        let command = command.as_std();
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--egress-interceptor", &proxy.address.to_string()]
        );
        let env = command.get_envs().collect::<Vec<_>>();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, "SMOLVM_INTERCEPTOR_TOKEN");
        assert_eq!(env[0].1.unwrap().len(), 64);
        assert!(
            !proxy
                .connection
                .environment
                .contains_key("SMOLVM_INTERCEPTOR_TOKEN")
        );
        Ok(())
    }
}
