"""A local translating gateway for coding agents on another vendor's models.

Claude Code speaks only the Anthropic Messages API and Codex only the OpenAI
Responses API. When the model comes from the other vendor, the eval runs a
LiteLLM proxy on the host for the length of the job: it serves both APIs,
forwards to the provider with that provider's own key, and translates the reply
back, streaming and tool calls included. Task containers reach the host
through Docker's bridge gateway address, which every Docker network can route
to.

The gateway serves TLS with a certificate it generates for that address.
Harbor's egress-control sidecar proxies plain HTTP at the HTTP layer and drops
a response whose headers take more than about 15 seconds, which a large prompt
to a slow model does routinely; TLS it passes through byte for byte. The agent
in the container has to trust the certificate: `ExoAgent` installs it into the
task container's store, and the Claude Code harness reads
`EXO_SANDBOX_CA_CERTS` into `NODE_EXTRA_CA_CERTS`.

Outside the eval, `python -m exo_harbor.gateway <model>` runs the same gateway
for a plain exo agent and prints the `exo model register` line to point at it.
"""

from __future__ import annotations

import argparse
import os
import signal
import socket
import ssl
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

GATEWAY_STARTUP_TIMEOUT_SEC = 90.0
HEALTH_POLL_SEC = 0.5
# Where ExoAgent installs the gateway's certificate inside a task container.
SANDBOX_CA_PATH = "/usr/local/share/ca-certificates/exo-gateway.crt"


class GatewayError(RuntimeError):
    """The gateway could not be started."""


def is_anthropic_model(model: str) -> bool:
    """Anthropic's models, by exo's own routing rule (`claude*`)."""
    return model.lower().startswith("claude")


def needs_gateway(harness: str, model: str) -> bool:
    """Whether `harness` cannot call `model`'s provider in its native API.

    exo's own harnesses translate for themselves; pi picks a provider from the
    model name. Only the vendor CLIs are locked to one wire format.
    """
    if harness == "claude-code":
        return not is_anthropic_model(model)
    if harness == "codex":
        return is_anthropic_model(model)
    return False


def docker_host_ip() -> str:
    """The host as seen from inside any Docker network on this machine."""
    result = subprocess.run(
        [
            "docker",
            "network",
            "inspect",
            "bridge",
            "-f",
            "{{(index .IPAM.Config 0).Gateway}}",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    address = result.stdout.strip()
    if result.returncode != 0 or not address:
        raise GatewayError(
            f"could not read Docker's bridge gateway address: {result.stderr.strip()}"
        )
    return address


def gateway_executable() -> Path:
    """The proxy's `litellm` command, installed next to this interpreter."""
    litellm = Path(sys.executable).parent / "litellm"
    if not litellm.is_file():
        raise GatewayError(
            f"{litellm} is missing; the eval's environment has to include "
            "litellm[proxy] (rerun eval.sh to install it)"
        )
    return litellm


def generate_certificate(address: str, directory: Path) -> tuple[Path, Path]:
    """A self-signed certificate for `address`, written into `directory`."""
    directory.mkdir(parents=True, exist_ok=True)
    cert, key = directory / "gateway-ca.crt", directory / "gateway-ca.key"
    result = subprocess.run(
        [
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", str(key), "-out", str(cert), "-days", "7",
            "-subj", f"/CN=exo gateway {address}",
            "-addext", f"subjectAltName=IP:{address}",
        ],
        capture_output=True, text=True, check=False,
    )
    if result.returncode != 0:
        raise GatewayError(f"openssl could not create the gateway certificate: {result.stderr.strip()}")
    return cert, key


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("0.0.0.0", 0))
        return sock.getsockname()[1]


def wait_for_health(url: str, timeout_sec: float, process: subprocess.Popen[bytes]) -> None:
    # The health check goes to 127.0.0.1, which the certificate does not name,
    # so it skips verification; the check is only "did the proxy come up".
    context = ssl._create_unverified_context() if url.startswith("https") else None
    deadline = time.monotonic() + timeout_sec
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise GatewayError(f"gateway exited with code {process.returncode} during startup")
        try:
            with urllib.request.urlopen(url, timeout=2, context=context) as response:
                if response.status == 200:
                    return
        except (urllib.error.URLError, OSError, TimeoutError):
            pass
        time.sleep(HEALTH_POLL_SEC)
    raise GatewayError(f"gateway did not become healthy within {timeout_sec:.0f}s")


@dataclass
class ModelGateway:
    process: subprocess.Popen[bytes]
    base_url: str
    log_path: Path
    ca_path: Path | None

    @classmethod
    def start(cls, model: str, log_path: Path, *, tls: bool = True) -> ModelGateway:
        """Run LiteLLM's proxy for `model` and return once it answers.

        LiteLLM reads the provider key from the provider's own environment
        variable (OPENAI_API_KEY, GEMINI_API_KEY, ...), so the eval's
        `--api-key-env` has to be that variable for the gateway path.
        """
        litellm = gateway_executable()
        address = docker_host_ip()
        port = free_port()
        log_path.parent.mkdir(parents=True, exist_ok=True)
        command = [
            str(litellm),
            "--model",
            model,
            "--host",
            "0.0.0.0",
            "--port",
            str(port),
            # Each CLI sends its own vendor's extras (Codex's
            # prompt_cache_key, for one); drop what the provider
            # cannot take instead of failing the request.
            "--drop_params",
        ]
        ca_path = None
        if tls:
            ca_path, key_path = generate_certificate(address, log_path.parent)
            command += ["--ssl_certfile_path", str(ca_path), "--ssl_keyfile_path", str(key_path)]
        scheme = "https" if tls else "http"
        with log_path.open("ab") as log:
            process = subprocess.Popen(
                command, stdout=log, stderr=subprocess.STDOUT, env=os.environ
            )
        # Codex appends `/responses` to an OpenAI-style `/v1` base URL, and the
        # Claude Code harness drops that segment before appending its own
        # `/v1/messages`, so one URL serves both.
        gateway = cls(
            process=process,
            base_url=f"{scheme}://{address}:{port}/v1",
            log_path=log_path,
            ca_path=ca_path,
        )
        try:
            wait_for_health(
                f"{scheme}://127.0.0.1:{port}/health/liveliness",
                GATEWAY_STARTUP_TIMEOUT_SEC,
                process,
            )
        except GatewayError as error:
            gateway.close()
            tail = log_path.read_text(errors="replace")[-2000:] if log_path.exists() else ""
            raise GatewayError(f"{error}; gateway log tail:\n{tail}") from error
        return gateway

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Run a translating gateway so Claude Code can use a non-Anthropic "
            "model, or Codex an Anthropic one. The provider key is read from "
            "its usual variable (OPENAI_API_KEY, ANTHROPIC_API_KEY, ...)."
        )
    )
    parser.add_argument("model", help="upstream model id, for example gpt-5.5")
    parser.add_argument(
        "--no-tls",
        action="store_true",
        help=(
            "serve plain HTTP. TLS is the default because Harbor's egress "
            "sidecar drops slow plain-HTTP responses; a plain exo Docker "
            "sandbox has no such proxy and needs no certificate"
        ),
    )
    parser.add_argument(
        "--log",
        type=Path,
        default=Path("gateway.log"),
        help="where the proxy writes its log (default: ./gateway.log)",
    )
    args = parser.parse_args()
    try:
        gateway = ModelGateway.start(args.model, args.log, tls=not args.no_tls)
    except GatewayError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    print(f"Gateway for {args.model}: {gateway.base_url} (log: {gateway.log_path})")
    if gateway.ca_path is not None:
        print(
            f"Certificate: {gateway.ca_path}. The agent in the sandbox has to trust "
            f"it: install it at {SANDBOX_CA_PATH} and run update-ca-certificates "
            "(Codex), and set EXO_SANDBOX_CA_CERTS to that path where exo runs "
            "(Claude Code)."
        )
    print("Register the model against it, then create the agent as usual:")
    print(
        f"  exo model register {args.model} --secret <secret> "
        f"--base-url {gateway.base_url}"
    )
    print("Press Ctrl-C to stop.", flush=True)
    try:
        signal.sigwait({signal.SIGINT, signal.SIGTERM})
    finally:
        gateway.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
