# Managed-agent sandbox images

The [publish workflow](../../.github/workflows/publish-sandbox-images.yml)
builds these images for Linux amd64 and arm64 and publishes them to GitHub
Container Registry:

- `ghcr.io/exoharness/codex-sandbox`
- `ghcr.io/exoharness/claude-code-sandbox`
- `ghcr.io/exoharness/pi-sandbox`

A change to an image or the workflow on `main` publishes `latest` and
`sha-<full source commit>`. A `sandbox-images-v*` Git tag also publishes the
tag's name and the source commit tag. The workflow can be run manually from
GitHub Actions. Each job prints the multi-platform image digest in its summary.
Use the `image@sha256:...` reference in bundled environment definitions after
the first successful publication so their contents match a tested image.

GitHub initially creates each container package as private, even when the
source repository is public. An organization package admin must change each
package's visibility to public in GitHub Packages before anonymous pulls work.
The images are linked to this repository by the OCI source label.

For local development, build an image directly from its directory:

```sh
docker build -t exo-codex-sandbox:latest exoharness/containers/codex-sandbox
docker build -t exo-claude-code-sandbox:latest exoharness/containers/claude-code-sandbox
docker build -t exo-pi-sandbox:latest exoharness/containers/pi-sandbox
```

Images contain harness executables and common command-line tools, not model
credentials or a project's dependencies. Store the model key in Exo's vault,
then reference its secret name in the agent spec. For example, create the
`openai` secret with:

```sh
exo vault secret create global openai --token-env OPENAI_API_KEY \
  --allow-origin https://api.openai.com
```

See [managed agents](../docs/managed-agents.md#vaults) for the credential
proxy and supported sandbox providers.
