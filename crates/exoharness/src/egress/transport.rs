use std::collections::HashSet;
use std::future::poll_fn;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use async_trait::async_trait;
use hickory_proto::op::{Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::{DNSClass, RData, Record, RecordType, rdata::A};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::IO_TIMEOUT;
use crate::types::canonical_egress_hosts;
use crate::{BoxSandboxTcpStream, SandboxEgressProxy};

const MAX_DNS_TASKS: usize = 32;
const DNS_BUFFER_SIZE: usize = 4096;
const DNS_BIND_ATTEMPTS: usize = 16;
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

const SYNTHETIC_IP: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);

/// The network side of a proxy: endpoint addresses and incoming byte streams.
/// Local Firecracker uses host sockets; Lima forwards those streams over its
/// bridge. This layer never receives credentials or the TLS signing key.
#[async_trait]
pub trait EgressTransport: Send + Sync {
    fn endpoints(&self) -> SandboxEgressProxy;
    /// Admit the VM only after acquisition has established its source address.
    async fn bind_source(&self, source_ip: Ipv4Addr) -> Result<()>;
    async fn accept(&self, tls: bool) -> Result<BoxSandboxTcpStream>;
    fn is_closed(&self) -> bool;
    /// Stop admission and initiate cleanup, including when called from Drop.
    fn close(&self);
    /// Wait for remote listeners to be released before reusing their ports.
    async fn shutdown(&self) -> Result<()> {
        self.close();
        Ok(())
    }
}

pub struct LocalEgressTransport {
    endpoints: SandboxEgressProxy,
    http: Socket<TcpListener>,
    https: Socket<TcpListener>,
    dns: Arc<Socket<UdpSocket>>,
    dns_tcp: Arc<Socket<TcpListener>>,
    // Guest IPv4 admitted by accepts_peer. Zero means unbound; bind_source
    // rejects 0.0.0.0 so the sentinel cannot collide with a real source.
    source: Arc<AtomicU32>,
    cancel: CancellationToken,
}

// Poll under a short lock instead of holding an Arc to the OS socket across
// await. close() can then drop the descriptor immediately and free its port.
struct Socket<T>(Mutex<Option<T>>);

impl<T> Socket<T> {
    fn new(socket: T) -> Self {
        Self(Mutex::new(Some(socket)))
    }

    fn poll<R>(&self, f: impl FnOnce(&T) -> Poll<io::Result<R>>) -> Poll<io::Result<R>> {
        match self.0.lock().expect("egress socket poisoned").as_ref() {
            Some(socket) => f(socket),
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "egress listener closed",
            ))),
        }
    }

    fn close(&self) {
        self.0.lock().expect("egress socket poisoned").take();
    }
}

impl Socket<TcpListener> {
    async fn accept(&self) -> io::Result<(tokio::net::TcpStream, SocketAddr)> {
        poll_fn(|cx| self.poll(|socket| socket.poll_accept(cx))).await
    }
}

impl Socket<UdpSocket> {
    async fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut buffer = ReadBuf::new(buffer);
        let peer = poll_fn(|cx| self.poll(|socket| socket.poll_recv_from(cx, &mut buffer))).await?;
        Ok((buffer.filled().len(), peer))
    }

    async fn send_to(&self, buffer: &[u8], peer: SocketAddr) -> io::Result<usize> {
        poll_fn(|cx| self.poll(|socket| socket.poll_send_to(cx, buffer, peer))).await
    }
}

async fn bind_dns(config: &crate::EgressListenConfig) -> io::Result<(TcpListener, UdpSocket)> {
    let address = if config.bind_address.is_unspecified() {
        config.advertised_address
    } else {
        config.bind_address
    };
    let mut attempt = 0;
    loop {
        attempt += 1;
        let tcp = TcpListener::bind((address, config.dns_port)).await?;
        match UdpSocket::bind(tcp.local_addr()?).await {
            Ok(udp) => return Ok((tcp, udp)),
            Err(error)
                if error.kind() == io::ErrorKind::AddrInUse
                    && config.dns_port == 0
                    && attempt < DNS_BIND_ATTEMPTS =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
}

impl LocalEgressTransport {
    pub async fn for_hosts(hosts: &[String]) -> Result<Self> {
        let probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
        probe.connect((SYNTHETIC_IP, 9)).await?;
        let IpAddr::V4(host_ip) = probe.local_addr()?.ip() else {
            return Err(anyhow!("egress requires host IPv4 routing"));
        };
        let hosts = canonical_egress_hosts(hosts)?;
        Self::bind(host_ip, &hosts).await
    }

    pub async fn with_config(config: crate::EgressListenConfig, hosts: &[String]) -> Result<Self> {
        let hosts = canonical_egress_hosts(hosts)?;
        Self::listen(config, &hosts).await
    }

    pub(super) async fn bind(host_ip: Ipv4Addr, hosts: &HashSet<String>) -> Result<Self> {
        Self::listen(
            crate::EgressListenConfig {
                bind_address: host_ip,
                advertised_address: host_ip,
                http_port: 0,
                https_port: 0,
                dns_port: 0,
            },
            hosts,
        )
        .await
    }

    async fn listen(config: crate::EgressListenConfig, hosts: &HashSet<String>) -> Result<Self> {
        let host_ip = config.advertised_address;
        let http = TcpListener::bind((config.bind_address, config.http_port)).await?;
        let https = TcpListener::bind((config.bind_address, config.https_port)).await?;
        let (dns_tcp, dns) = bind_dns(&config).await?;
        let endpoints = SandboxEgressProxy {
            http: SocketAddrV4::new(host_ip, http.local_addr()?.port()),
            https: SocketAddrV4::new(host_ip, https.local_addr()?.port()),
            dns: SocketAddrV4::new(host_ip, dns.local_addr()?.port()),
        };
        endpoints.validate()?;
        let source = Arc::new(AtomicU32::new(0));
        let cancel = CancellationToken::new();
        let dns = Arc::new(Socket::new(dns));
        let dns_tcp = Arc::new(Socket::new(dns_tcp));
        tokio::spawn(serve_dns(
            dns.clone(),
            dns_tcp.clone(),
            hosts.clone(),
            source.clone(),
            cancel.clone(),
        ));
        Ok(Self {
            endpoints,
            http: Socket::new(http),
            https: Socket::new(https),
            dns,
            dns_tcp,
            source,
            cancel,
        })
    }
}

#[async_trait]
impl EgressTransport for LocalEgressTransport {
    fn endpoints(&self) -> SandboxEgressProxy {
        self.endpoints
    }

    async fn bind_source(&self, source_ip: Ipv4Addr) -> Result<()> {
        ensure!(
            !source_ip.is_unspecified() && !source_ip.is_multicast() && !source_ip.is_broadcast(),
            "invalid proxy source address"
        );
        ensure!(!self.is_closed(), "egress listener closed");
        self.source
            .compare_exchange(0, u32::from(source_ip), Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| anyhow!("proxy source identity is already bound"))?;
        Ok(())
    }

    async fn accept(&self, tls: bool) -> Result<BoxSandboxTcpStream> {
        let listener = if tls { &self.https } else { &self.http };
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(anyhow!("egress listener closed")),
                incoming = listener.accept() => {
                    match incoming {
                        Ok((stream, peer)) if accepts_peer(&self.source, peer) => return Ok(Box::pin(stream)),
                        Ok(_) => {},
                        Err(error) => {
                            tracing::debug!(%error, "egress accept failed; retrying");
                            if !retry_after_error(&self.cancel).await {
                                return Err(anyhow!("egress listener closed"));
                            }
                        }
                    }
                }
            }
        }
    }

    fn is_closed(&self) -> bool {
        self.cancel.is_cancelled()
    }

    fn close(&self) {
        self.cancel.cancel();
        self.http.close();
        self.https.close();
        self.dns.close();
        self.dns_tcp.close();
    }
}

impl Drop for LocalEgressTransport {
    fn drop(&mut self) {
        self.close();
    }
}

fn accepts_peer(source: &AtomicU32, peer: SocketAddr) -> bool {
    let IpAddr::V4(ip) = peer.ip() else {
        return false;
    };
    let source = source.load(Ordering::SeqCst);
    source != 0 && source == u32::from(ip)
}

// Back off briefly after a listener error. Returns false once cancelled.
async fn retry_after_error(cancel: &CancellationToken) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(ACCEPT_RETRY_DELAY) => true,
    }
}

async fn serve_dns(
    dns: Arc<Socket<UdpSocket>>,
    tcp: Arc<Socket<TcpListener>>,
    hosts: HashSet<String>,
    source: Arc<AtomicU32>,
    cancel: CancellationToken,
) {
    let hosts = Arc::new(hosts);
    let mut tasks = JoinSet::new();
    let mut buffer = [0u8; DNS_BUFFER_SIZE];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if !matches!(result, Ok(Ok(()))) { tracing::debug!("egress DNS connection failed"); }
            }
            incoming = dns.recv_from(&mut buffer) => {
                let (size, peer) = match incoming {
                    Ok(incoming) => incoming,
                    Err(error) => {
                        tracing::debug!(%error, "egress DNS receive failed; retrying");
                        if retry_after_error(&cancel).await {
                            continue;
                        }
                        break;
                    }
                };
                if !accepts_peer(&source, peer) { continue; }
                if let Ok(answer) = dns_response(&hosts, &buffer[..size])
                    && dns.send_to(&answer, peer).await.is_err() {
                    tracing::debug!("egress DNS response failed");
                }
            }
            incoming = tcp.accept() => {
                let (mut stream, peer) = match incoming {
                    Ok(incoming) => incoming,
                    Err(error) => {
                        tracing::debug!(%error, "egress DNS accept failed; retrying");
                        if retry_after_error(&cancel).await {
                            continue;
                        }
                        break;
                    }
                };
                if !accepts_peer(&source, peer) || tasks.len() >= MAX_DNS_TASKS { continue; }
                let hosts = hosts.clone();
                tasks.spawn(async move {
                    tokio::time::timeout(IO_TIMEOUT, async {
                        let size = stream.read_u16().await?;
                        ensure!(usize::from(size) <= DNS_BUFFER_SIZE, "DNS request too large");
                        let mut bytes = vec![0; size as usize];
                        stream.read_exact(&mut bytes).await?;
                        let answer = dns_response(&hosts, &bytes)?;
                        stream.write_u16(answer.len().try_into()?).await?;
                        stream.write_all(&answer).await?;
                        Ok::<_, anyhow::Error>(())
                    }).await?
                });
            }
        }
    }
    tasks.shutdown().await;
}
pub(super) fn dns_response(hosts: &HashSet<String>, bytes: &[u8]) -> Result<Vec<u8>> {
    let query = Message::from_vec(bytes)?;
    ensure!(
        query.message_type() == MessageType::Query
            && query.op_code() == OpCode::Query
            && query.queries().len() == 1,
        "unsupported DNS query"
    );
    let question = &query.queries()[0];
    let mut response = Message::new();
    response
        .set_id(query.id())
        .set_message_type(MessageType::Response)
        .set_recursion_desired(query.recursion_desired())
        .set_recursion_available(false)
        .add_query(question.clone());
    let host = question
        .name()
        .to_ascii()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if question.query_class() != DNSClass::IN {
        response.set_response_code(ResponseCode::Refused);
    } else if !hosts.contains(&host) {
        response.set_response_code(ResponseCode::NXDomain);
    } else if question.query_type() == RecordType::A {
        response.add_answer(Record::from_rdata(
            question.name().clone(),
            0,
            RData::A(A(SYNTHETIC_IP)),
        ));
    }
    Ok(response.to_vec()?)
}
