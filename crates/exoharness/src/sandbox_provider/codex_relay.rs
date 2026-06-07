use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const PROCESS_PIPE_BUFFER_SIZE: usize = 64 * 1024;
pub(crate) const PATH: &str = "/tmp/exo-codex-relay.py";
pub(crate) const RECV_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const CLIENT_TIMEOUT: Duration = Duration::from_secs(40);

pub(crate) const SCRIPT: &str = r###"
import json
import os
import queue
import shutil
import socket
import socketserver
import subprocess
import sys
import threading

HOST = "127.0.0.1"
PORT = 48765
LOG_PATH = "/tmp/exo-codex-relay.log"
APP_STDERR_PATH = "/tmp/codex-app-server.stderr"


def log(message):
    with open(LOG_PATH, "a", buffering=1) as relay_log:
        print(message, file=relay_log)


def ensure_codex(env):
    path = env.get("PATH") or os.environ.get("PATH") or ""
    codex = shutil.which("codex", path=path)
    if codex:
        return codex
    npm = shutil.which("npm", path=path)
    if not npm:
        raise RuntimeError("codex is not installed and npm is not available to install @openai/codex")
    prefix = os.path.join(env["CODEX_HOME"], "npm")
    cache = os.path.join(env["CODEX_HOME"], "npm-cache")
    os.makedirs(prefix, exist_ok=True)
    os.makedirs(cache, exist_ok=True)
    env["NPM_CONFIG_PREFIX"] = prefix
    env["NPM_CONFIG_CACHE"] = cache
    env["PATH"] = os.path.join(prefix, "bin") + os.pathsep + path
    log("codex binary not found; installing @openai/codex")
    with open(LOG_PATH, "a", buffering=1) as relay_log:
        subprocess.run(
            [npm, "install", "-g", "@openai/codex"],
            env=env,
            stdout=relay_log,
            stderr=relay_log,
            check=True,
        )
    codex = shutil.which("codex", path=env["PATH"])
    if not codex:
        raise RuntimeError("@openai/codex installed but codex is still not on PATH")
    return codex


class RelayState:
    def __init__(self):
        home = os.environ.get("HOME") or "/tmp/exo-home"
        codex_home = os.environ.get("CODEX_HOME") or "/tmp/exo-codex-home"
        workdir = os.environ.get("CODEX_WORKDIR") or os.path.join(home, "workspace")
        os.makedirs(home, exist_ok=True)
        os.makedirs(codex_home, exist_ok=True)
        os.makedirs(workdir, exist_ok=True)
        env = os.environ.copy()
        env["HOME"] = home
        env["CODEX_HOME"] = codex_home
        env["CODEX_WORKDIR"] = workdir
        env.setdefault("SHELL", "/bin/bash")
        codex = ensure_codex(env)
        stderr = open(APP_STDERR_PATH, "a", buffering=1)
        self.process = subprocess.Popen(
            [codex, "app-server", "--listen", "stdio://"],
            cwd=workdir,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr,
            text=True,
            bufsize=1,
        )
        self.messages = queue.Queue()
        self.write_lock = threading.Lock()
        threading.Thread(target=self._read_stdout, daemon=True).start()

    def _read_stdout(self):
        try:
            for line in self.process.stdout:
                line = line.rstrip("\r\n")
                if line:
                    self.messages.put({"message": json.loads(line)})
        except Exception as exc:
            self.messages.put({"error": f"Codex relay stdout reader failed: {exc}"})
        finally:
            code = self.process.poll()
            self.messages.put({"error": f"Codex app-server exited: {code}"})

    def send(self, message):
        if self.process.poll() is not None:
            raise RuntimeError(f"Codex app-server is not running: {self.process.returncode}")
        payload = json.dumps(message, separators=(",", ":")) + "\n"
        with self.write_lock:
            self.process.stdin.write(payload)
            self.process.stdin.flush()

    def recv(self, timeout):
        if self.process.poll() is not None:
            raise RuntimeError(f"Codex app-server is not running: {self.process.returncode}")
        try:
            item = self.messages.get(timeout=timeout)
        except queue.Empty:
            return {"ok": True, "timeout": True}
        if "error" in item:
            raise RuntimeError(item["error"])
        return {"ok": True, "message": item["message"]}


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
            elif kind == "send":
                STATE.send(request["message"])
                response = {"ok": True}
            elif kind == "recv":
                response = STATE.recv(float(request.get("timeout_seconds", 30)))
            else:
                response = {"ok": False, "error": f"unknown relay request type: {kind}"}
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        self.request.sendall(json.dumps(response, separators=(",", ":")).encode("utf-8"))


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True


def server():
    global STATE
    log("starting exo codex relay")
    STATE = RelayState()
    with Server((HOST, PORT), Handler) as srv:
        srv.serve_forever()


def client():
    request = sys.argv[2] if len(sys.argv) > 2 else os.environ.get("EXO_CODEX_RELAY_REQUEST")
    if not request:
        raise SystemExit("EXO_CODEX_RELAY_REQUEST is required")
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
    os.environ["EXO_CODEX_RELAY_REQUEST"] = request
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
    message: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<f64>,
}

impl Request {
    pub(crate) fn ping() -> Self {
        Self {
            kind: "ping",
            message: None,
            timeout_seconds: None,
        }
    }

    fn recv() -> Self {
        Self {
            kind: "recv",
            message: None,
            timeout_seconds: Some(RECV_TIMEOUT.as_secs_f64()),
        }
    }

    fn send(message: Value) -> Self {
        Self {
            kind: "send",
            message: Some(message),
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
    pub(crate) message: Option<Value>,
    #[serde(default)]
    pub(crate) error: Option<String>,
}

#[async_trait]
pub(crate) trait Client: Send + Sync + 'static {
    async fn request(&self, request: Request) -> Result<Response>;
}

pub(crate) fn is_codex_app_server_command(command: &crate::SandboxCommand) -> bool {
    command.argv.iter().any(|arg| {
        arg.contains("codex app-server") && arg.contains("--listen") && arg.contains("stdio://")
    })
}

pub(crate) fn install_script_shell_command() -> String {
    format!(
        "python3 -c {} {} {}",
        shell_quote(
            "import pathlib,sys; path=pathlib.Path(sys.argv[1]); path.write_text(sys.argv[2]); path.chmod(0o700)"
        ),
        shell_quote(PATH),
        shell_quote(SCRIPT),
    )
}

pub(crate) fn stop_shell_command() -> String {
    "pkill -f '[e]xo-codex-relay.py' >/dev/null 2>&1 || true; pkill -f '[c]odex app-server --listen stdio://' >/dev/null 2>&1 || true".to_string()
}

pub(crate) fn server_argv() -> Vec<String> {
    vec![
        "python3".to_string(),
        PATH.to_string(),
        "server".to_string(),
    ]
}

pub(crate) fn server_shell_command() -> String {
    format!("python3 {} server", shell_quote(PATH))
}

pub(crate) fn client_argv(request_json: String) -> Vec<String> {
    vec![
        "python3".to_string(),
        PATH.to_string(),
        "client".to_string(),
        request_json,
    ]
}

pub(crate) fn client_shell_command(request_json: &str) -> String {
    format!(
        "python3 {} client {}",
        shell_quote(PATH),
        shell_quote(request_json),
    )
}

pub(crate) fn process_parts(client: Arc<dyn Client>) -> crate::SandboxProcessParts {
    let (stdout_reader, stdout_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stderr_reader, stderr_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stdin_reader, stdin_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (error_tx, mut error_rx) = mpsc::unbounded_channel();

    spawn_stdout_poller(
        Arc::clone(&client),
        stdout_writer,
        stderr_writer,
        error_tx.clone(),
    );
    spawn_stdin_forwarder(client, stdin_reader, error_tx);

    let wait: BoxFuture<'static, crate::Result<i32>> = Box::pin(async move {
        match error_rx.recv().await {
            Some(error) => Err(error),
            None => Ok(0),
        }
    });

    crate::SandboxProcessParts {
        stdout: Box::pin(stdout_reader.compat()),
        stderr: Box::pin(stderr_reader.compat()),
        stdin: Box::pin(stdin_writer.compat_write()),
        wait,
    }
}

fn spawn_stdout_poller(
    client: Arc<dyn Client>,
    stdout_writer: tokio::io::DuplexStream,
    stderr_writer: tokio::io::DuplexStream,
    error_tx: mpsc::UnboundedSender<anyhow::Error>,
) {
    tokio::spawn(async move {
        if let Err(error) = poll_stdout(client, stdout_writer, stderr_writer).await {
            if error_tx.send(error).is_err() {
                tracing::debug!("Codex relay waiter stopped before stdout error could be reported");
            }
        }
    });
}

fn spawn_stdin_forwarder(
    client: Arc<dyn Client>,
    stdin_reader: tokio::io::DuplexStream,
    error_tx: mpsc::UnboundedSender<anyhow::Error>,
) {
    tokio::spawn(async move {
        if let Err(error) = forward_stdin(client, stdin_reader).await {
            if error_tx.send(error).is_err() {
                tracing::debug!("Codex relay waiter stopped before stdin error could be reported");
            }
        }
    });
}

async fn poll_stdout(
    client: Arc<dyn Client>,
    mut stdout_writer: tokio::io::DuplexStream,
    mut stderr_writer: tokio::io::DuplexStream,
) -> Result<()> {
    loop {
        let response = client.request(Request::recv()).await?;
        if response.timeout {
            continue;
        }
        let Some(message) = response.message else {
            continue;
        };
        let mut line = serde_json::to_vec(&message).context("encoding Codex relay message")?;
        line.push(b'\n');
        match stdout_writer.write_all(&line).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
            Err(error) => return Err(error).context("writing Codex relay stdout"),
        }
        if let Err(error) = stdout_writer.flush().await {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error).context("flushing Codex relay stdout");
        }
        if let Err(error) = stderr_writer.flush().await {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error).context("flushing Codex relay stderr");
        }
    }
}

async fn forward_stdin(
    client: Arc<dyn Client>,
    stdin_reader: tokio::io::DuplexStream,
) -> Result<()> {
    let mut reader = BufReader::new(stdin_reader);
    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .await
            .context("reading Codex relay stdin")?;
        if bytes_read == 0 {
            return Ok(());
        }
        let message = line.trim_end_matches(['\r', '\n']);
        if message.trim().is_empty() {
            continue;
        }
        let message: Value =
            serde_json::from_str(message).context("decoding Codex relay stdin message")?;
        client.request(Request::send(message)).await?;
    }
}

pub(crate) fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '=' | ',')
        })
    {
        return arg.to_string();
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(c);
        }
    }
    quoted.push('\'');
    quoted
}
