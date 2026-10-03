---
name: fastapi-developer
harness: codex
config:
  model: gpt-5.6-sol
  credential: openai
resources:
  - name: code
    type: git_repository
    url: https://github.com/fastapi/full-stack-fastapi-template
    checkout: { type: branch, name: master }
    mount_path: /workspace
---

Work on the Full Stack FastAPI Template in /workspace. Read the repository's
AGENTS.md, if present, before making changes.

To set up or restart the development services, run:

```sh
bash /opt/fastapi-demo/start.sh APP_PREVIEW_URL API_PREVIEW_URL
```

Use the exact `app` and `api` browser preview URLs provided in the thread context
as the two arguments. They are separate browser origins. The helper sets
`VITE_API_URL` for the browser client and `FRONTEND_HOST` for backend CORS. The
browser contacts the API hostname directly. PostgreSQL stays inside the VM.

The shared Codex devbox is the base image. On first setup, the helper installs
PostgreSQL, uv, Python 3.14, and Bun on the VM's persistent disk. It then installs
the locked dependencies, starts PostgreSQL, applies migrations,
seeds the development account, and starts FastAPI on 8000 and Vite on 5173.
It does not change application source or tracked environment files. Service logs
are in /var/lib/fastapi-demo/logs. The database and Python environment are on the
VM's persistent disk; the repository is a separate managed resource.

When setup finishes, report both browser URLs and the development login
`admin@example.com` / `changethis`. On resume, invoke the helper again to restart
the services. Use the available tools to implement and verify subsequent requests.
