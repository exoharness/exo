"""A local translating gateway for coding agents on another vendor's models.

Claude Code speaks only the Anthropic Messages API and Codex only the OpenAI
Responses API. When the model comes from the other vendor, the eval runs a
LiteLLM proxy on the host for the length of the job: it serves both APIs,
forwards to the provider with that provider's own key, and translates the reply
back, streaming and tool calls included.
Task containers reach the host through Docker's bridge gateway address, which
every Docker network can route to.

The proxy lives in its own virtualenv beside the eval's: litellm[proxy] pins
`rich` below 14 while harbor needs 14.1 or newer, so the two cannot share one.

Outside the eval, `python -m exo_harbor.gateway <model>` runs the same gateway
for a plain exo agent and prints the `exo model register` line to point at it.
"""

from __future__ import annotations

import argparse
import os
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

GATEWAY_STARTUP_TIMEOUT_SEC = 90.0
HEALTH_POLL_SEC = 0.5
# The version the Messages translation (tools and streaming included) was
# verified against.
GATEWAY_REQUIREMENT = "litellm[proxy]==1.101.0"
GATEWAY_VENV = Path(__file__).resolve().parents[2] / ".gateway-venv"
INSTALL_TIMEOUT_SEC = 600


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


def gateway_executable(venv: Path = GATEWAY_VENV) -> Path:
    """The proxy's `litellm` command, installing its virtualenv on first use."""
    litellm = venv / "bin" / "litellm"
    if litellm.is_file():
        return litellm
    print(f"Installing {GATEWAY_REQUIREMENT} into {venv}", flush=True)
    try:
        subprocess.run([sys.executable, "-m", "venv", str(venv)], check=True)
        subprocess.run(
            [str(venv / "bin" / "pip"), "install", "--quiet", GATEWAY_REQUIREMENT],
            check=True,
            timeout=INSTALL_TIMEOUT_SEC,
        )
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        raise GatewayError(f"installing the gateway failed: {error}") from error
    if not litellm.is_file():
        raise GatewayError(f"{litellm} is still missing after installing {GATEWAY_REQUIREMENT}")
    return litellm


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("0.0.0.0", 0))
        return sock.getsockname()[1]


def wait_for_health(url: str, timeout_sec: float, process: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + timeout_sec
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise GatewayError(f"gateway exited with code {process.returncode} during startup")
        try:
            with urllib.request.urlopen(url, timeout=2) as response:
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

    @classmethod
    def start(cls, model: str, log_path: Path) -> ModelGateway:
        """Run LiteLLM's proxy for `model` and return once it answers.

        LiteLLM reads the provider key from the provider's own environment
        variable (OPENAI_API_KEY, GEMINI_API_KEY, ...), so the eval's
        `--api-key-env` has to be that variable for the gateway path.
        """
        litellm = gateway_executable()
        port = free_port()
        log_path.parent.mkdir(parents=True, exist_ok=True)
        with log_path.open("ab") as log:
            process = subprocess.Popen(
                [
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
                ],
                stdout=log,
                stderr=subprocess.STDOUT,
                env=os.environ,
            )
        # Codex appends `/responses` to an OpenAI-style `/v1` base URL, and the
        # Claude Code harness drops that segment before appending its own
        # `/v1/messages`, so one URL serves both.
        gateway = cls(
            process=process,
            base_url=f"http://{docker_host_ip()}:{port}/v1",
            log_path=log_path,
        )
        try:
            wait_for_health(
                f"http://127.0.0.1:{port}/health/liveliness",
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
        "--log",
        type=Path,
        default=Path("gateway.log"),
        help="where the proxy writes its log (default: ./gateway.log)",
    )
    args = parser.parse_args()
    try:
        gateway = ModelGateway.start(args.model, args.log)
    except GatewayError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    print(f"Gateway for {args.model}: {gateway.base_url} (log: {gateway.log_path})")
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
