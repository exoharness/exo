# Filesystem resources

Declare resources on an agent. Exo starts preparing each thread's private copies
in the background while the CLI accepts input. The first prompt or sandbox command
waits for any remaining preparation before execution. Resuming a thread keeps its
existing files, including across sandbox replacement.

```yaml
resources:
  - name: code
    type: git_repository
    url: https://github.com/exoharness/exo
    checkout: { type: branch, name: main }
    mount_path: /workspace
  - name: fixtures
    type: directory
    path: ./testdata
    mount_path: /fixtures
    mode: ro
```

```sh
exo agent create exo-dev --file agent.md
exo agent run --agent exo-dev
exo agent run --agent exo-dev --thread THREAD
```

Resource paths are absolute inside the sandbox. With resources, the default
working directory is `/workspace`; an environment's `default_workdir` overrides
it. Mount destinations must not overlap other resources or environment mounts.
Resources always use a separate sandbox for each thread. Sandbox processes trust
the mounted Git resource paths via `safe.directory`, so host/guest ownership
differences do not block Git. This leaves host Git configuration unchanged and
does not trust unrelated repositories.

## Git cache

For a Git URL, Exo maintains a checkout on a cached volume. For a new thread,
preparation fetches the remote, advances the checkout to the requested branch or
commit, unmounts the volume, and makes a copy-on-write clone. The Git database and
working tree are reused across updates. Refresh and clone are serialized for
that cache; threads never mount the cache itself. Existing thread copies keep
their own files and Git state while later cache updates change only shared
blocks that need writing.

Omitting `checkout` follows the remote's default branch. Pin a commit with
`checkout: {type: commit, sha: FULL_COMMIT_SHA}`. A failed fetch fails the command
before execution instead of silently using an old checkout. Resuming an existing thread
does not fetch and works when the source is offline.

Preparation uses the Git credential helpers and configuration on the runtime
host, so an existing Git login also works for private repositories. With an HTTP
provider, these are the server's credentials, not the CLI machine's. Host Git
configuration and credentials are not copied into thread volumes.

To use a vault credential instead, add `credential: github` to the resource and
select a vault containing that secret:

```sh
exo vault secret create personal github --allow-origin https://github.com \
  --token-env GITHUB_TOKEN
exo agent run --agent exo-dev --vault personal
```

The credential must target the Git server's HTTPS origin. If you created the
secret without `--allow-origin`, update it without changing its ID:

```sh
exo vault secret update personal github --allow-origin https://github.com
```

Preparation uses HTTP Basic authentication with username `x-access-token`,
with host credential helpers disabled for that request. Credentials stay on the
host; they are not saved in Git configuration or copied into thread volumes.
Caches are partitioned by URL, checkout and vault credential identity.
Git commands inside the sandbox use a placeholder credential through Exo's
egress proxy; the real credential stays outside the sandbox. GitHub resources
also expose a placeholder as `GH_TOKEN` for `gh` when the credential policy
permits `https://api.github.com`. `exo vault login personal --preset github`
authorizes both GitHub origins. The credential's permissions
control repository access; this does not add a separate Git push approval policy.

## Local sources

`type: directory` copies a local directory, including untracked files. A local
Git checkout can use `type: git_repository` with `path` instead of `url`; this
also makes its Git metadata independent and preserves staged/unstaged changes.
Local paths resolve relative to the agent file. The HTTP runtime accepts absolute
paths on its own host; it does not upload the client's directory.

Local sources are prepared when creating or updating the agent. Preparation
walks file metadata to reuse an unchanged snapshot. New threads clone that
snapshot without scanning the original directory. To capture subsequent local
edits, run `exo agent update exo-dev --file agent.md`. Existing threads keep their
previous workspace. Exo's runtime state is excluded; sources containing the vault
master key are rejected. `--mount` remains a live host mount.

## Storage and lifecycle

On macOS, the cache and thread copies are sparse APFS disk images. On Linux,
resource copies require a filesystem supporting reflinks. Exo requires CoW and
reports an error instead of silently making a full copy. Prepared images have a
1 TiB virtual capacity on macOS; physical allocation grows with written data.

Apple Containers, Docker, Firecracker, SmolVM and local-process sandboxes can use these
resources. A local-process sandbox does not isolate the filesystem or enforce
read-only access. Cloud sandbox resource transfer is not implemented.

Firecracker stores sparse ext4 resource disks on an XFS host volume with
`reflink=1`. New threads reflink a whole disk without walking its files, then
attach the private disk to the microVM. Cached volumes are mounted only during
preparation; disks modified by agents are mounted only inside their microVM.
Read-only resources use read-only drives and mounts. Disk capacity uses
`--firecracker-workspace-size-gib` (20 GiB by default).

On macOS this storage lives inside Lima. Host Git credentials are resolved on
macOS and sent over the local bridge for preparation, never into the agent VM.
Local directories are exported to an archive and imported into a cached disk
once per prepared snapshot; later threads reuse that disk.
See [Firecracker setup](../support/firecracker/README.md#resource-storage) for
XFS provisioning. Firecracker requires the matching Exo guest initramfs.

On macOS, SmolVM also prepares local `docker save` images (`.tar`, `.tar.gz`,
`.tgz`) once under `<root>/cache/smolvm/images`. Preparation uses an offline VM;
subsequent threads share a read-only APFS base with private writable overlays.
The cache is keyed by archive contents and survives Exo restarts. No SmolVM
patch or extra configuration is needed. Registry images use SmolVM's own image
handling. Image caches, like resource caches, are not automatically evicted.

Deleting a thread terminates its sandboxes, unmounts and removes its private
volumes. Deleting an agent does this for all its threads. Shared preparation
caches remain available for reuse; automatic cache eviction is not implemented.
Thread forking with resources and attaching resources to an existing thread are
not yet supported.
