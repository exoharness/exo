---
name: exo-developer
harness: codex
config:
  model: gpt-5.6-sol
resources:
  - name: code
    type: git_repository
    url: https://github.com/exoharness/exo
    checkout: { type: branch, name: main }
    mount_path: /workspace
    credential: github-git
---

Work on Exo in /workspace. Read the repository's AGENTS.md before making changes.
Use the available development tools to implement and verify the user's request.
