# SWE-bench Lite

The 300-instance SWE-bench Lite split as Harbor tasks. Harbor's registry has
SWE-bench Verified but not Lite, so `generate.py` builds the task directories
here from the HuggingFace dataset and the prebuilt SWE-bench images:

```bash
./generate.py                                    # all 300 tasks
./generate.py --limit 10                         # first 10, by instance id
./generate.py --instance-id django__django-11099 # one task; repeatable
```

It is a self-contained `uv run` script: uv fetches the pinned `swebench`
package, which supplies each repository's test command and the per-instance
image name. Each task is rendered from `task-template/`:

| File                     | Purpose                                                                                                            |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| `instruction.md`         | the issue text, with a short preamble saying the repository is at `/testbed` and hidden tests grade a code change. |
| `task.toml`              | Harbor config (w/ `network_mode = "no-network"` to prevent upstream fix fetching)                                  |
| `environment/Dockerfile` | `FROM` the prebuilt `swebench/sweb.eval.x86_64.*` image, plus uv for the verifier.                                 |
| `tests/test.sh`          | resets and applies the hidden test patch, runs the tests, and grades the log with SWE-bench's parser.              |
| `tests/config.json`      | the raw dataset record the grader reads.                                                                           |
| `solution/solve.sh`      | applies the gold patch, for Harbor's oracle agent.                                                                 |

The test-script construction follows Harbor's own swebench adapter, which
mirrors SWE-bench's evaluation script. Each task's first run pulls a
multi-gigabyte image from Docker Hub.

Note: Exo's `web_search` and `web_fetch` tools run on the host, outside the
container, so the task's network policy does not cover them.

The generated task directories are gitignored; only the recipe is tracked.
Run the eval with:

```bash
../../eval.sh --dataset=swebench-lite --n-tasks=3
```
