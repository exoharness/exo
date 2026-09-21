# SWE-bench Lite

The 300-instance SWE-bench Lite split as Harbor tasks. Harbor's registry has
SWE-bench Verified but not Lite, so `generate.sh` builds the task directories
locally from Harbor's own swebench adapter:

```bash
./generate.sh                                  # all 300 tasks
./generate.sh --limit 10                       # first 10, alphabetical
./generate.sh --instance-id django__django-11099
```

It clones the adapter at a pinned commit into `.upstream/`, applies
`adapter-dataset-flag.patch`, and writes one directory per instance here.
The patch makes three changes to the upstream adapter:

- a `--dataset` flag, since upstream hardcodes SWE-bench Verified;
- a short preamble in `instruction.md` telling the agent the repository is
  at `/testbed` and that hidden tests grade a code change (upstream passes
  the raw issue text, which an agent can mistake for a question);
- `network_mode = "no-network"` for the agent phase, so the agent cannot
  fetch the upstream fix or the issue thread. The verifier keeps public
  access because its grading script installs the swebench parser with uv.
  Harbor switches the container's policy between phases through its
  egress-control sidecar, which needs nftables fib support in the Docker
  host's kernel. Note that Exo's `web_search` and `web_fetch` tools run on
  the host, outside the container, so the task policy does not cover them. Each task's
  Dockerfile starts from the prebuilt `swebench/sweb.eval.x86_64.*` image on
  Docker Hub, so the first run of each task pulls a multi-gigabyte image.

The generated directories are gitignored. Run the eval with:

```bash
../../eval.sh --dataset=swebench-lite --n-tasks=3
```
