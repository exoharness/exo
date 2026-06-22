"""Exo installed agent for Harbor (Terminal-Bench 2.0).

Installs a slim exo bundle (binary + pruned node_modules + minimal shell harness)
into the task container and drives it with `exo conversation send`, configured
with a local-process sandbox so exo's shell == the task container's shell.

Modeled on harbor.agents.installed.openclaw (the closest in-tree analog: a
local-first, config-driven CLI agent), but invoking exo's CLI.

Run it via:
  harbor run -d terminal-bench@2.0 \
    --agent-import-path exo_agent.agent:ExoAgent \
    -m openai/gpt-5.5 --ae OPENAI_API_KEY=$OPENAI_API_KEY -l 1
(with PYTHONPATH including this dir and EXO_BUNDLE pointing at exo-bundle.tar.gz)
"""

from __future__ import annotations

import os
import shlex
from typing import Any

from harbor.agents.installed.base import BaseInstalledAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


class ExoAgent(BaseInstalledAgent):
    SUPPORTS_ATIF: bool = False

    _BUNDLE_REMOTE = "/tmp/exo-bundle.tar.gz"
    _EXO_HOME = "/opt/exo"
    _EXO_ROOT = "/tmp/exoroot"
    _HARNESS = "/opt/exo/examples/simple-coding-agent/harness.ts"
    _OUTPUT = "/logs/agent/exo.txt"

    def __init__(self, *args: Any, exo_bundle: str | None = None, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        _default_bundle = os.path.join(
            os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
            "exo-bundle.tar.gz",
        )
        self._exo_bundle = exo_bundle or os.environ.get("EXO_BUNDLE", _default_bundle)

    @staticmethod
    def name() -> str:
        return "exo"

    def version(self) -> str | None:
        return self._version or "dev"

    @property
    def _model(self) -> str:
        # Harbor passes e.g. "openai/gpt-5.5"; exo registers the bare model name.
        return self._parsed_model_name or (self.model_name or "gpt-5.5")

    def _exo(self) -> str:
        return f"exo --root {self._EXO_ROOT} --secret-backend file"

    async def install(self, environment: BaseEnvironment) -> None:
        # System deps + Node 22 (skip node if already present).
        await self.exec_as_root(
            environment,
            "apt-get update && apt-get install -y --no-install-recommends "
            "curl ca-certificates tar",
            env={"DEBIAN_FRONTEND": "noninteractive"},
        )
        await self.exec_as_root(
            environment,
            "command -v node >/dev/null 2>&1 || { "
            "curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && "
            "apt-get install -y nodejs; }",
            env={"DEBIAN_FRONTEND": "noninteractive"},
        )
        # Ship + unpack the slim exo bundle; expose `exo` on PATH (world-readable).
        await environment.upload_file(self._exo_bundle, self._BUNDLE_REMOTE)
        await self.exec_as_root(
            environment,
            f"mkdir -p {self._EXO_HOME} && "
            f"tar xzf {self._BUNDLE_REMOTE} -C {self._EXO_HOME} && "
            f"chmod -R a+rX {self._EXO_HOME} && "
            f"ln -sf {self._EXO_HOME}/target/x86_64-unknown-linux-musl/release/exo "
            "/usr/local/bin/exo",
        )

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> Any:
        key = self._get_env("OPENAI_API_KEY") or ""
        env = {"OPENAI_API_KEY": key}
        exo = self._exo()
        # Run exo from the bundle dir so tsx resolves @exo/harness via tsconfig.json.
        cd = f"cd {self._EXO_HOME} && "

        # Discover the task working directory so the agent's shell operates there.
        pwd_res = await self.exec_as_agent(environment, "pwd")
        taskdir = (getattr(pwd_res, "stdout", None) or "/").strip() or "/"

        # One-time exo setup: model + agent (minimal shell harness, local-process).
        await self.exec_as_agent(
            environment, f"{cd}{exo} secret set openai --env OPENAI_API_KEY", env=env
        )
        await self.exec_as_agent(
            environment,
            f"{cd}{exo} model register {shlex.quote(self._model)} --secret openai",
            env=env,
        )
        await self.exec_as_agent(
            environment,
            f"{cd}{exo} agent create --slug t --model {shlex.quote(self._model)} "
            f"--harness {self._HARNESS} --sandbox-provider local-process Exo",
            env=env,
        )
        await self.exec_as_agent(environment, f"{cd}{exo} conversation create t c", env=env)
        # Point exo's shell at the task dir (local-process => host path == container path).
        await self.exec_as_agent(
            environment,
            f"{cd}{exo} conversation mount add t c {shlex.quote(taskdir)} {shlex.quote(taskdir)}",
            env=env,
        )

        # Drive the task; capture transcript for Harbor.
        await self.exec_as_agent(
            environment,
            f"mkdir -p /logs/agent; {cd}{exo} conversation send t c "
            f"{shlex.quote(instruction)} 2>&1 | tee {self._OUTPUT}",
            env=env,
        )

        # Harvest exo's recorded token usage + cost from its event store BEFORE
        # the container is torn down, and write it to the agent logs so the
        # report can show tokens/$ per task. Best-effort: never fail the task.
        try:
            await self.exec_as_agent(environment, self._USAGE_SCRIPT, env=env)
        except Exception as e:  # noqa: BLE001
            self.logger.debug("exo usage harvest skipped: %s", e)

    # node is installed in install(); exo records a `usage` block per turn in its
    # event JSONs — including its own cost_usd. We trust exo's cost_usd (it prices
    # turns itself; no hardcoded price table to rot) and only sum the token fields
    # for the per-task breakdown (prompt/completion/cached/reasoning) that a single
    # cost figure can't give.
    _USAGE_SCRIPT = r"""
node -e '
const fs=require("fs");
function walk(d){let o=[];let es;try{es=fs.readdirSync(d,{withFileTypes:true});}catch(e){return o;}for(const e of es){const p=d+"/"+e.name;if(e.isDirectory())o=o.concat(walk(p));else if(e.name.endsWith(".json"))o.push(p);}return o;}
let p=0,c=0,ca=0,r=0,cost=0;
const files=walk("/tmp/exoroot").filter(f=>f.includes("/events/"));
const sum=(t,re)=>{let s=0,m;const rx=new RegExp(re,"g");while(m=rx.exec(t))s+=+m[1];return s;};
for(const f of files){let t;try{t=fs.readFileSync(f,"utf8");}catch(e){continue;}
  p+=sum(t,"\"prompt_tokens\":\\s*(\\d+)");c+=sum(t,"\"completion_tokens\":\\s*(\\d+)");
  ca+=sum(t,"\"prompt_cached_tokens\":\\s*(\\d+)");r+=sum(t,"\"completion_reasoning_tokens\":\\s*(\\d+)");
  cost+=sum(t,"\"cost_usd\":\\s*([0-9.]+)");}
const out={event_files:files.length,prompt_tokens:p,completion_tokens:c,cached_tokens:ca,
  reasoning_tokens:r,cost_usd:Math.round(cost*1e6)/1e6,cost_source:"exo usage.cost_usd"};
fs.mkdirSync("/logs/agent",{recursive:true});
fs.writeFileSync("/logs/agent/exo_usage.json",JSON.stringify(out,null,2));
console.log("exo_usage",JSON.stringify(out));
'
""".strip()
