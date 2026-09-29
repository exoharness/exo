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

- Try to reproduce the issue.
- Clearly explain whether you were able to reproduce it.
- If reproduced, provide a minimal test case.
- When possible, suggest a temporary workaround.
- Attempt to fix the underlying issue, test the fix, and provide a patch.
