---
title: Support agent tutorial
description: TODO
---

# Support agent tutorial

Let’s walk through how to build an increasingly capable support agent, step by step.

## Before starting

Make sure Exo is installed (<TODO: installation instructions>).

We’ll use OpenAI models with Codex for this tutorial, so we need to make sure they are authorized. Exo stores secrets in Vaults, which allow an agent to use them (without seeing them). Every thread has access to the `global` vault, so let’s populate an OpenAI API key there:

```bash
exo vault secret create global --preset openai
```

- Make sure the environment variable `OPENAI_API_KEY` is set
- On Mac OS X this will capture the environment variable and save it to the keychain
- `create --preset openai` is short-hand for `create openai --token-env OPENAI_API_KEY --allow-origin https://api.openai.com`

## 1. Ticket triage

We’ll start with a ticket triage bot that has no external dependencies. Define the agent in a file called `support-analyst.md` .

```markdown
---
name: support-analyst
harness: codex
model:
  name: gpt-6-sol
  credential: openai
---

For each support ticket, determine whether it describes a technical issue.
If it does:

* Try to reproduce the issue.
* Clearly explain whether you were able to reproduce it.
* If reproduced, provide a minimal test case.
* When possible, suggest a temporary workaround.

You do not have any github credentials, so if you need to access github stuff
use public APIs (eg https://api.github.com/repos/{owner}/{repo}/issues/{number}).
```

- Exo supports popular harnesses like Codex, Claude Code, and Pi, as well as custom harnesses that implement the `Harness` interface.
- By default agents run locally in a smolvm sandbox with unrestricted egress. Codex agents default to ghcr.io/exoharness/codex-devbox which comes with python/typescript.
- The `openai` credential gets substituted as a placeholder in the actual sandbox. If you want to customize the sandbox, you can define an Environment.

Now, run the agent

```bash
exo agent run --agent-file support-analyst.md
```

Paste in an example case, like:

```bash
Check out https://github.com/braintrustdata/autoevals/issues/223
```

The agent will reproduce the issue and provide a diagnosis!

## 2. Adding git context

Support agents get even more powerful when they can access your source code. Let’s start by connecting the agent to GitHub. If you have the `gh` cli installed, run

```bash
exo vault secret create global --preset github
```

or, if you have a personal access token

```bash
exo vault secret create global github \
  --token-env GITHUB_PAT \
  --allow-origin https://github.com
  --allow-origin https://api.github.com
```

Then, update the agent definition to include the git repo you’d like to triage.

```bash
---
name: support-analyst
harness: codex
model:
  name: gpt-6-sol
  credential: openai
resources:
  - name: autoevals
    type: git_repository
    url: https://github.com/braintrustdata/autoevals
---

For each support ticket, determine whether it describes a technical issue.
If it does:

* Try to reproduce the issue.
* Clearly explain whether you were able to reproduce it.
* If reproduced, provide a minimal test case.
* When possible, suggest a temporary workaround.
* Attempt to fix the underlying issue, test the fix, and provide a patch.
```

And try out the same prompt again:

```bash
Check out https://github.com/braintrustdata/autoevals/issues/223
```
