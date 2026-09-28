# Managed-agent sandbox images

The [publish workflow](../../.github/workflows/publish-sandbox-images.yml)
builds these images for Linux amd64 and arm64 and publishes them to GitHub
Container Registry:

- `ghcr.io/exoharness/codex-sandbox`
- `ghcr.io/exoharness/codex-devbox`
- `ghcr.io/exoharness/claude-code-sandbox`
- `ghcr.io/exoharness/pi-sandbox`

`codex-sandbox` is the small default with Codex, Node.js, git, curl, and
ripgrep. `codex-devbox` uses the same Codex version and adds Python 3 with pip
and venv, plus pnpm, TypeScript, and tsx. Project dependencies are installed
separately.

Publishing is manual. Run the workflow from the Actions tab on `main` and
approve its `sandbox-image-publish` environment before any image is pushed.
That environment is configured in GitHub with `main` as its deployment branch
and `ankrgyl` as its required reviewer.
The workflow publishes `latest` and `sha-<full source commit>`; optionally
provide a `sandbox-images-vX.Y.Z` release tag. Each job prints the
multi-platform image digest in its summary.
Harness defaults and bundled environment definitions use the published
`image@sha256:...` references so they keep using the same tested images when
`latest` changes. Update those references after testing a new publication.

GitHub initially creates each container package as private, even when the
source repository is public. An organization owner must allow public packages
in the organization's Packages settings, then a package admin must change each
package's visibility to public before anonymous pulls work. The images are
linked to this repository by the OCI source label.

For local development, build the images directly:

```sh
docker build -t exo-codex-sandbox:latest exoharness/containers/codex-sandbox
docker build --build-arg DEVBOX_TOOLS=true -t exo-codex-devbox:latest exoharness/containers/codex-sandbox
docker build -t exo-claude-code-sandbox:latest exoharness/containers/claude-code-sandbox
docker build -t exo-pi-sandbox:latest exoharness/containers/pi-sandbox
```

Store the model key in Exo's vault, then reference its secret name in the agent
spec. For example, create the
`openai` secret with:

```sh
exo vault secret create global openai --token-env OPENAI_API_KEY \
  --allow-origin https://api.openai.com
```

See [managed agents](../docs/managed-agents.md#vaults) for the credential
proxy and supported sandbox providers.
