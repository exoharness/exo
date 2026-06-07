use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const PROCESS_PIPE_BUFFER_SIZE: usize = 64 * 1024;
pub(crate) const PATH: &str = "/tmp/exo-process-bridge.py";
pub(crate) const RECV_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) const SCRIPT: &str = r###"
import base64
import json
import os
import queue
import socket
import socketserver
import subprocess
import sys
import threading

HOST = "127.0.0.1"
PORT = 48765
LOG_PATH = "/tmp/exo-process-bridge.log"


def log(message):
    with open(LOG_PATH, "a", buffering=1) as bridge_log:
        print(message, file=bridge_log)


def decode_env_json():
    raw = os.environ.get("EXO_PROCESS_BRIDGE_ENV_JSON")
    if not raw:
        return {}
    decoded = json.loads(raw)
    if not isinstance(decoded, dict):
        raise RuntimeError("EXO_PROCESS_BRIDGE_ENV_JSON must be an object")
    return {str(key): str(value) for key, value in decoded.items()}


def decode_argv_json():
    raw = os.environ.get("EXO_PROCESS_BRIDGE_ARGV_JSON")
    if not raw:
        raise RuntimeError("EXO_PROCESS_BRIDGE_ARGV_JSON is required")
    decoded = json.loads(raw)
    if not isinstance(decoded, list) or not decoded:
        raise RuntimeError("EXO_PROCESS_BRIDGE_ARGV_JSON must be a non-empty array")
    return [str(value) for value in decoded]


class BridgeState:
    def __init__(self):
        argv = decode_argv_json()
        cwd = os.environ.get("EXO_PROCESS_BRIDGE_CWD") or None
        env = os.environ.copy()
        env.update(decode_env_json())
        log("starting process bridge command")
        self.process = subprocess.Popen(
            argv,
            cwd=cwd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        self.events = queue.Queue()
        self.write_lock = threading.Lock()
        threading.Thread(target=self._read_stream, args=("stdout", self.process.stdout), daemon=True).start()
        threading.Thread(target=self._read_stream, args=("stderr", self.process.stderr), daemon=True).start()
        threading.Thread(target=self._wait, daemon=True).start()

    def _read_stream(self, stream, handle):
        try:
            while True:
                data = handle.read(65536)
                if not data:
                    break
                self.events.put({
                    "type": stream,
                    "data": base64.b64encode(data).decode("ascii"),
                })
        except Exception as exc:
            self.events.put({"type": "error", "message": f"{stream} reader failed: {exc}"})

    def _wait(self):
        try:
            exit_code = self.process.wait()
            self.events.put({"type": "exit", "exit_code": exit_code})
        except Exception as exc:
            self.events.put({"type": "error", "message": f"wait failed: {exc}"})

    def write(self, data):
        if self.process.poll() is not None:
            raise RuntimeError(f"bridged process is not running: {self.process.returncode}")
        payload = base64.b64decode(data.encode("ascii"))
        with self.write_lock:
            self.process.stdin.write(payload)
            self.process.stdin.flush()

    def close_stdin(self):
        with self.write_lock:
            if self.process.stdin:
                self.process.stdin.close()

    def recv(self, timeout):
        try:
            event = self.events.get(timeout=timeout)
        except queue.Empty:
            return {"ok": True, "timeout": True}
        if event.get("type") == "error":
            raise RuntimeError(event.get("message") or "bridged process failed")
        return {"ok": True, "event": event}


STATE = None


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        global STATE
        data = b""
        while True:
            chunk = self.request.recv(1024 * 1024)
            if not chunk:
                break
            data += chunk
        try:
            request = json.loads(data.decode("utf-8") or "{}")
            kind = request.get("type")
            if kind == "ping":
                response = {"ok": True}
            elif kind == "write":
                STATE.write(request["data"])
                response = {"ok": True}
            elif kind == "close_stdin":
                STATE.close_stdin()
                response = {"ok": True}
            elif kind == "recv":
                response = STATE.recv(float(request.get("timeout_seconds", 30)))
            else:
                response = {"ok": False, "error": f"unknown bridge request type: {kind}"}
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        self.request.sendall(json.dumps(response, separators=(",", ":")).encode("utf-8"))


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True


def server():
    global STATE
    log("starting exo process bridge")
    STATE = BridgeState()
    with Server((HOST, PORT), Handler) as srv:
        srv.serve_forever()


def client():
    request = sys.argv[2] if len(sys.argv) > 2 else os.environ.get("EXO_PROCESS_BRIDGE_REQUEST")
    if not request:
        raise SystemExit("EXO_PROCESS_BRIDGE_REQUEST is required")
    with socket.create_connection((HOST, PORT), timeout=35) as sock:
        sock.sendall(request.encode("utf-8"))
        sock.shutdown(socket.SHUT_WR)
        chunks = []
        while True:
            chunk = sock.recv(1024 * 1024)
            if not chunk:
                break
            chunks.append(chunk)
    sys.stdout.write(b"".join(chunks).decode("utf-8"))


def ping():
    request = json.dumps({"type": "ping"}, separators=(",", ":"))
    os.environ["EXO_PROCESS_BRIDGE_REQUEST"] = request
    client()


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "client"
    if mode == "server":
        server()
    elif mode == "ping":
        ping()
    elif mode == "client":
        client()
    else:
        raise SystemExit(f"unknown mode: {mode}")
"###;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Request {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<f64>,
}

impl Request {
    pub(crate) fn ping() -> Self {
        Self {
            kind: "ping",
            data: None,
            timeout_seconds: None,
        }
    }

    fn recv() -> Self {
        Self {
            kind: "recv",
            data: None,
            timeout_seconds: Some(RECV_TIMEOUT.as_secs_f64()),
        }
    }

    fn write(data: &[u8]) -> Self {
        Self {
            kind: "write",
            data: Some(STANDARD.encode(data)),
            timeout_seconds: None,
        }
    }

    fn close_stdin() -> Self {
        Self {
            kind: "close_stdin",
            data: None,
            timeout_seconds: None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct Response {
    pub(crate) ok: bool,
    #[serde(default)]
    pub(crate) timeout: bool,
    #[serde(default)]
    pub(crate) event: Option<Event>,
    #[serde(default)]
    pub(crate) error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Event {
    Stdout { data: String },
    Stderr { data: String },
    Exit { exit_code: i32 },
}

#[async_trait]
pub(crate) trait Client: Send + Sync + 'static {
    async fn request(&self, request: Request) -> Result<Response>;
}

pub(crate) fn install_script_shell_command() -> String {
    format!(
        "python3 -c {} {} {}",
        super::shell_quote(
            "import pathlib,sys; path=pathlib.Path(sys.argv[1]); path.write_text(sys.argv[2]); path.chmod(0o700)"
        ),
        super::shell_quote(PATH),
        super::shell_quote(SCRIPT),
    )
}

pub(crate) fn stop_shell_command() -> String {
    format!(
        "pkill -f {} >/dev/null 2>&1 || true",
        super::shell_quote("[e]xo-process-bridge.py"),
    )
}

pub(crate) fn server_shell_command() -> String {
    format!("python3 {} server", super::shell_quote(PATH))
}

pub(crate) fn client_argv(request_json: String) -> Vec<String> {
    vec![
        "python3".to_string(),
        PATH.to_string(),
        "client".to_string(),
        request_json,
    ]
}

pub(crate) fn process_parts(client: Arc<dyn Client>) -> crate::SandboxProcessParts {
    let (stdout_reader, stdout_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stderr_reader, stderr_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stdin_reader, stdin_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (wait_tx, wait_rx) = oneshot::channel();
    let (error_tx, mut error_rx) = mpsc::unbounded_channel();

    spawn_output_poller(
        Arc::clone(&client),
        stdout_writer,
        stderr_writer,
        wait_tx,
        error_tx.clone(),
    );
    spawn_stdin_forwarder(client, stdin_reader, error_tx);

    let wait: BoxFuture<'static, crate::Result<i32>> = Box::pin(async move {
        tokio::select! {
            result = wait_rx => match result {
                Ok(exit_code) => Ok(exit_code),
                Err(_) => Err(anyhow::anyhow!("process bridge output poller stopped")),
            },
            error = error_rx.recv() => match error {
                Some(error) => Err(error),
                None => Err(anyhow::anyhow!("process bridge stopped before exit")),
            },
        }
    });

    crate::SandboxProcessParts {
        stdout: Box::pin(stdout_reader.compat()),
        stderr: Box::pin(stderr_reader.compat()),
        stdin: Box::pin(stdin_writer.compat_write()),
        wait,
    }
}

fn spawn_output_poller(
    client: Arc<dyn Client>,
    stdout_writer: tokio::io::DuplexStream,
    stderr_writer: tokio::io::DuplexStream,
    wait_tx: oneshot::Sender<i32>,
    error_tx: mpsc::UnboundedSender<anyhow::Error>,
) {
    tokio::spawn(async move {
        if let Err(error) = poll_output(client, stdout_writer, stderr_writer, wait_tx).await
            && error_tx.send(error).is_err()
        {
            tracing::debug!("process bridge waiter stopped before output error could be reported");
        }
    });
}

fn spawn_stdin_forwarder(
    client: Arc<dyn Client>,
    stdin_reader: tokio::io::DuplexStream,
    error_tx: mpsc::UnboundedSender<anyhow::Error>,
) {
    tokio::spawn(async move {
        if let Err(error) = forward_stdin(client, stdin_reader).await
            && error_tx.send(error).is_err()
        {
            tracing::debug!("process bridge waiter stopped before stdin error could be reported");
        }
    });
}

async fn poll_output(
    client: Arc<dyn Client>,
    mut stdout_writer: tokio::io::DuplexStream,
    mut stderr_writer: tokio::io::DuplexStream,
    wait_tx: oneshot::Sender<i32>,
) -> Result<()> {
    loop {
        let response = client.request(Request::recv()).await?;
        if response.timeout {
            continue;
        }
        let Some(event) = response.event else {
            continue;
        };
        match event {
            Event::Stdout { data } => {
                write_bridge_output(&mut stdout_writer, &data, "stdout").await?;
            }
            Event::Stderr { data } => {
                write_bridge_output(&mut stderr_writer, &data, "stderr").await?;
            }
            Event::Exit { exit_code } => {
                if wait_tx.send(exit_code).is_err() {
                    tracing::debug!("process bridge waiter dropped before exit could be reported");
                }
                return Ok(());
            }
        }
    }
}

async fn write_bridge_output(
    writer: &mut tokio::io::DuplexStream,
    data: &str,
    stream: &str,
) -> Result<()> {
    let decoded = STANDARD
        .decode(data)
        .with_context(|| format!("decoding process bridge {stream}"))?;
    match writer.write_all(&decoded).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("writing process bridge {stream}"));
        }
    }
    match writer.flush().await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error).with_context(|| format!("flushing process bridge {stream}")),
    }
}

async fn forward_stdin(
    client: Arc<dyn Client>,
    mut stdin_reader: tokio::io::DuplexStream,
) -> Result<()> {
    let mut buffer = vec![0; 16 * 1024];
    loop {
        let bytes_read = stdin_reader
            .read(&mut buffer)
            .await
            .context("reading process bridge stdin")?;
        if bytes_read == 0 {
            client.request(Request::close_stdin()).await?;
            return Ok(());
        }
        client
            .request(Request::write(&buffer[..bytes_read]))
            .await
            .context("writing process bridge stdin")?;
    }
}
