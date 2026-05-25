# Runtime requirements

This document lists what needs to be installed and configured on the host to
run `exo` and its optional features. The aim is one place to look when a
feature surfaces an "X not available" error.

## Building

- Rust 1.95+ (workspace `rust-version`)
- pnpm 10.x + Node 22.x for the TypeScript bits (`pnpm install && pnpm check`)

## Backends — sandbox

The sandbox backend is selected with `--sandbox-backend <kind>` or the
`EXO_SANDBOX_BACKEND` env var. Defaults are cfg-based:

Linux → `docker`
macOS → `apple-container`

| Backend           | Host requirement                                                 | Notes                                                                                                                                        |
| ----------------- | ---------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `local-process`   | none                                                             | Commands run directly on the host. No isolation. Useful for tests.                                                                           |
| `docker`          | docker CLI + a running docker daemon (`docker info` succeeds)    | Linux runners have this preinstalled. On macOS use Colima or Docker Desktop.                                                                 |
| `apple-container` | Apple `container` CLI installed and `container system start` run | Apple Silicon, macOS 15+. Install with `sudo installer -pkg container-*-installer-signed.pkg -target /` from a release of `apple/container`. |

## Backends — secret

Selected with `--secret-backend <kind>` or `EXO_SECRET_BACKEND`.

| Backend          | Host requirement                           | Notes                                                                                                                                        |
| ---------------- | ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `file`           | none                                       | Master key at `$XDG_CONFIG_HOME/exo/master.key` (default `~/.config/exo/master.key`), 0600. Path can be overridden with `--master-key-path`. |
| `apple-keychain` | macOS, access to the user's login keychain | Headless contexts (CI, ssh without a graphical login) may fail to unlock.                                                                    |

## Optional: full-state sandbox snapshots (`/checkpoint`)

The default `/snapshot` slash command captures filesystem state only and has
no requirements beyond the chosen sandbox backend. The `/checkpoint` command
captures full state (filesystem + running processes + memory + open file
descriptors) and is backed by CRIU on the Docker backend. It needs:

- **CRIU on the host.** The docker daemon shells out to `criu` during
  `docker checkpoint create`. On Linux:

  Ubuntu 22.04 LTS: `sudo apt install criu`
  Ubuntu 24.04 LTS: not in the official repos; build from
  <https://github.com/checkpoint-restore/criu>
  or pull from a 22.04 mirror
  Fedora / RHEL: `sudo dnf install criu`

  `sudo criu check` should report "Looks good" for the kernel to be
  supported.

- **Docker experimental mode.** Add to `/etc/docker/daemon.json`:

  ```json
  { "experimental": true }
  ```

  Then `sudo systemctl restart docker`.

- **Passwordless `sudo` for the user running `exo`.** The docker daemon
  writes CRIU dump files as root (mode 0600), so the snapshot path needs
  one `sudo chown` to claim them before tarring. Restore stays user-level.
  Typical dev workstations already have passwordless sudo; for a hardened
  machine, add a sudoers fragment:

  ```
  # /etc/sudoers.d/exo-criu  (edit with `sudo visudo -f /etc/sudoers.d/exo-criu`)
  yourname ALL=(root) NOPASSWD: /usr/bin/chown
  ```

- **Apple Silicon macOS / `apple-container` backend.** Not yet implemented
  end-to-end. The underlying VZ framework supports `pause` +
  `saveMachineStateTo:`, but Apple's `container` CLI does not yet expose
  that. Tracked in the snapshot design doc.

When the requirements are missing, `/checkpoint` surfaces an actionable
error message pointing back here, rather than a cryptic raw-CLI failure.

## Optional: CI integration tests

The integration test workflow (`.github/workflows/integration.yml`) runs the
real `exo` binary against a wiremock-backed fake OpenAI Responses endpoint
on a real sandbox. It self-skips a matrix cell when the runner doesn't have
the required backend:

| Matrix cell             | Runner           | Setup performed by the workflow             |
| ----------------------- | ---------------- | ------------------------------------------- |
| `linux/local-process`   | `ubuntu-latest`  | none                                        |
| `linux/docker`          | `ubuntu-latest`  | docker preinstalled                         |
| `macos/local-process`   | `macos-15`       | none                                        |
| `macos/docker`          | `macos-15-intel` | `crazy-max/ghaction-setup-docker` (Colima)  |
| `macos/apple-container` | _none_           | requires a self-hosted runner (nested-virt) |

See <https://github.com/actions/runner-images/issues/13505> for the
nested-virt limitation that blocks `macos/apple-container` on GitHub-hosted
runners.
