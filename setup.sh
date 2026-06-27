#!/usr/bin/env bash
set -euo pipefail

REPO_URL="${EXO_REPO_URL:-https://github.com/ankrgyl/exo.git}"
REPO_REF="${EXO_REPO_REF:-main}"
INSTALL_DIR="${EXO_INSTALL_DIR:-$PWD}"
MODEL_NAME="${EXO_MODEL:-gpt-5.4}"
UPSTREAM_MODEL="${EXO_UPSTREAM_MODEL:-$MODEL_NAME}"
AGENT_NAME="${EXO_AGENT_NAME:-Exo}"
USER_NAME="${EXO_USER_NAME:-}"
FORCE_INSTALL="${EXO_SETUP_FORCE:-false}"

die() {
  echo "error: $*" >&2
  exit 1
}

info() {
  echo
  echo "==> $*"
}

usage() {
  cat <<'EOF'
Usage:
  bash setup.sh [options]

Options:
  --branch <branch>     Git branch to clone for testing (default: main)
  --ref <ref>           Git ref to clone; alias for --branch
  --repo-ref <ref>      Git ref to clone; alias for --branch
  --force               Back up existing files and install into a non-empty
                        directory; refuses to run directly in $HOME or /
  --help                Show this help

Environment overrides:
  EXO_REPO_URL, EXO_REPO_REF, EXO_INSTALL_DIR, EXO_MODEL, EXO_UPSTREAM_MODEL,
  EXO_AGENT_NAME, EXO_USER_NAME, EXOCLAW_LOCAL_PROMPT_FILE, EXO_SETUP_FORCE
EOF
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --branch|--ref|--repo-ref)
        REPO_REF="${2:-}"
        [[ -n "$REPO_REF" ]] || die "$1 requires a value"
        shift 2
        ;;
      --force)
        FORCE_INSTALL=true
        shift
        ;;
      --help|-h)
        usage
        exit 0
        ;;
      *)
        die "unknown option: $1"
        ;;
    esac
  done
}

require_command() {
  local command="$1"
  local install_hint="$2"
  if ! command -v "$command" >/dev/null 2>&1; then
    die "$command is required. $install_hint"
  fi
}

check_dependencies() {
  local missing=()

  command -v git >/dev/null 2>&1 ||
    missing+=("git")
  command -v node >/dev/null 2>&1 ||
    missing+=("node")
  command -v pnpm >/dev/null 2>&1 ||
    missing+=("pnpm")
  command -v cargo >/dev/null 2>&1 ||
    missing+=("cargo")
  command -v rustc >/dev/null 2>&1 ||
    missing+=("rustc")
  command -v docker >/dev/null 2>&1 ||
    missing+=("docker")

  if ((${#missing[@]} > 0)); then
    echo "Missing required dependencies:" >&2
    local item
    for item in "${missing[@]}"; do
      echo "  - $item" >&2
    done
    print_dependency_install_help "${missing[@]}"
    exit 1
  fi
}

print_dependency_install_help() {
  echo >&2
  echo "Install the missing dependencies, then rerun this setup script." >&2
  case "$(uname -s)" in
    Darwin)
      print_macos_dependency_install_help "$@"
      ;;
    Linux)
      print_linux_dependency_install_help "$@"
      ;;
    *)
      print_generic_dependency_install_help "$@"
      ;;
  esac
}

print_macos_dependency_install_help() {
  echo >&2
  echo "macOS install commands:" >&2
  if ! command -v brew >/dev/null 2>&1; then
    echo "  # Install Homebrew first if you do not have it:" >&2
    echo '  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"' >&2
  fi
  if missing_has git "$@"; then
    echo "  xcode-select --install  # includes Git, if Apple developer tools are missing" >&2
  fi
  local brew_packages=()
  if missing_has node "$@"; then
    brew_packages+=("node")
  fi
  if missing_has pnpm "$@"; then
    brew_packages+=("pnpm")
  fi
  if missing_has cargo "$@" || missing_has rustc "$@"; then
    echo '  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y' >&2
  fi
  if ((${#brew_packages[@]} > 0)); then
    echo "  brew install ${brew_packages[*]}" >&2
  fi
  if missing_has docker "$@"; then
    echo "  brew install --cask docker" >&2
    echo "  open -a Docker" >&2
  fi
}

print_linux_dependency_install_help() {
  echo >&2
  echo "Linux install commands:" >&2
  echo "  # Ubuntu/Debian example:" >&2
  echo "  sudo apt-get update" >&2
  local apt_packages=()
  if missing_has git "$@"; then
    apt_packages+=("git")
  fi
  if missing_has node "$@"; then
    echo "  curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -" >&2
    apt_packages+=("nodejs")
  fi
  if missing_has cargo "$@" || missing_has rustc "$@"; then
    apt_packages+=("curl" "build-essential" "pkg-config" "libssl-dev")
  fi
  if ((${#apt_packages[@]} > 0)); then
    echo "  sudo apt-get install -y ${apt_packages[*]}" >&2
  fi
  if missing_has pnpm "$@"; then
    echo "  corepack enable pnpm" >&2
  fi
  if missing_has cargo "$@" || missing_has rustc "$@"; then
    echo '  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y' >&2
    echo '  source "$HOME/.cargo/env"' >&2
  fi
  if missing_has docker "$@"; then
    echo "  # Install Docker Engine for your distro, then start it:" >&2
    echo "  # https://docs.docker.com/engine/install/" >&2
  fi
}

print_generic_dependency_install_help() {
  echo >&2
  echo "Install Git, Node.js 22+, pnpm, Rust via rustup, and Docker Desktop/Engine." >&2
  echo "  Git: https://git-scm.com/downloads" >&2
  echo "  Node.js: https://nodejs.org/" >&2
  echo "  pnpm: https://pnpm.io/installation" >&2
  echo "  Rust: https://rustup.rs/" >&2
  echo "  Docker: https://www.docker.com/products/docker-desktop/" >&2
}

missing_has() {
  local needle="$1"
  shift
  local item
  for item in "$@"; do
    if [[ "$item" == "$needle" ]]; then
      return 0
    fi
  done
  return 1
}

is_exo_checkout() {
  local dir="$1"
  [[ -d "$dir/.git" && -f "$dir/scripts/exo.sh" ]]
}

prompt_yes_no() {
  local prompt="$1"
  local default="$2"
  local suffix
  local answer
  case "$default" in
    y|Y) suffix="Y/n" ;;
    n|N) suffix="y/N" ;;
    *) die "invalid yes/no default: $default" ;;
  esac
  while true; do
    read -r -p "$prompt [$suffix]: " answer
    answer="${answer:-$default}"
    case "$answer" in
      y|Y|yes|YES) return 0 ;;
      n|N|no|NO) return 1 ;;
      *) echo "Please answer yes or no." ;;
    esac
  done
}

read_secret() {
  local prompt="$1"
  local value
  read -r -s -p "$prompt: " value
  echo >&2
  printf '%s' "$value"
}

prompt_text() {
  local prompt="$1"
  local default="$2"
  local value
  read -r -p "$prompt [$default]: " value
  printf '%s' "${value:-$default}"
}

env_value() {
  local key="$1"
  local file="$2"
  if [[ ! -f "$file" ]]; then
    return
  fi
  awk -F= -v key="$key" '$1 == key { print substr($0, length(key) + 2); found = 1; exit } END { if (!found) exit 1 }' "$file" 2>/dev/null || true
}

set_env_value() {
  local key="$1"
  local value="$2"
  local file="$3"
  local tmp
  [[ "$value" != *$'\n'* ]] || die "$key cannot contain newlines"
  tmp="$(mktemp)"
  if [[ -f "$file" ]] && grep -qE "^${key}=" "$file"; then
    awk -v key="$key" -v value="$value" '
      BEGIN { updated = 0 }
      $0 ~ "^" key "=" {
        print key "=" value
        updated = 1
        next
      }
      { print }
      END {
        if (!updated) {
          print key "=" value
        }
      }
    ' "$file" >"$tmp"
  else
    [[ -f "$file" ]] && cp "$file" "$tmp"
    if [[ -s "$tmp" ]]; then
      printf '\n' >>"$tmp"
    fi
    printf '%s=%s\n' "$key" "$value" >>"$tmp"
  fi
  mv "$tmp" "$file"
  chmod 600 "$file"
}

prompt_env_secret() {
  local key="$1"
  local file="$2"
  local description="$3"
  local required="$4"
  local existing
  local env_value
  local value
  existing="$(env_value "$key" "$file")"
  if [[ -n "$existing" ]]; then
    echo "$key is already set in .env; using it."
    return
  fi
  env_value="${!key:-}"
  if [[ -n "$env_value" ]]; then
    if prompt_yes_no "$key is set in your shell environment. Use it for .env?" y; then
      set_env_value "$key" "$env_value" "$file"
      return
    fi
  fi
  while true; do
    value="$(read_secret "$description")"
    if [[ -n "$value" || "$required" != true ]]; then
      break
    fi
    echo "$key is required for the default Exo model setup."
  done
  if [[ -n "$value" ]]; then
    set_env_value "$key" "$value" "$file"
  fi
}

ensure_docker_running() {
  require_command docker "Install Docker Desktop: https://www.docker.com/products/docker-desktop/"
  if docker info >/dev/null 2>&1; then
    return
  fi
  if [[ "$(uname -s)" == "Darwin" ]] && command -v open >/dev/null 2>&1; then
    echo "Docker does not appear to be running. Opening Docker Desktop..."
    open -a Docker || true
    for _ in $(seq 1 60); do
      if docker info >/dev/null 2>&1; then
        return
      fi
      sleep 2
    done
  fi
  die "Docker is not running. Start Docker Desktop, then rerun this script."
}

trust_mise_config() {
  local config="$1"
  if [[ ! -f "$config" ]] || ! command -v mise >/dev/null 2>&1; then
    return
  fi
  echo "Trusting local mise config: $config"
  mise trust "$config"
}

write_local_profile() {
  local file="$1"
  local user_name="$2"
  mkdir -p "$(dirname "$file")"
  {
    echo "# Local Exo Profile"
    echo
    echo "This file is local to this machine and should not be committed."
    if [[ -n "$user_name" ]]; then
      echo
      echo "The user's name is $user_name."
    fi
  } >"$file"
  chmod 600 "$file"
}

choose_install_dir() {
  if is_exo_checkout "$PWD"; then
    echo "Using current Exo checkout: $PWD" >&2
    printf '%s' "$PWD"
    return
  fi
  printf '%s' "$INSTALL_DIR"
}

directory_can_receive_checkout() {
  local dir="$1"
  local entry
  shopt -s nullglob dotglob
  for entry in "$dir"/*; do
    case "$(basename "$entry")" in
      .|..|setup.sh|.DS_Store) ;;
      *) return 1 ;;
    esac
  done
  return 0
}

prepare_non_empty_install_dir() {
  local dir="$1"
  if directory_can_receive_checkout "$dir"; then
    return
  fi
  if [[ "$FORCE_INSTALL" != true ]]; then
    echo "error: $dir is not empty, and setup will not overwrite existing files." >&2
    echo >&2
    echo "To continue, choose one of:" >&2
    echo "  - Run setup from a new empty directory:" >&2
    echo "      mkdir -p ~/exo && cd ~/exo && bash /path/to/setup.sh" >&2
    echo "  - Or set an explicit install directory:" >&2
    echo "      EXO_INSTALL_DIR=~/exo bash setup.sh" >&2
    echo "  - Or, if this is a failed/throwaway setup directory, rerun with:" >&2
    echo "      bash setup.sh --force" >&2
    exit 1
  fi
  backup_existing_install_dir "$dir"
}

backup_existing_install_dir() {
  local dir="$1"
  local resolved_dir
  resolved_dir="$(cd "$dir" && pwd -P)"
  if [[ "$resolved_dir" == "/" || "$resolved_dir" == "$HOME" ]]; then
    die "refusing --force in $resolved_dir. Create an empty directory or set EXO_INSTALL_DIR instead."
  fi

  local backup_dir entry name parent_dir base_name
  parent_dir="$(dirname "$resolved_dir")"
  base_name="$(basename "$resolved_dir")"
  backup_dir="$parent_dir/$base_name.exo-setup-backup-$(date +%Y%m%d%H%M%S)"
  mkdir -p "$backup_dir"
  shopt -s nullglob dotglob
  for entry in "$dir"/*; do
    name="$(basename "$entry")"
    case "$name" in
      .|..|setup.sh|.DS_Store) ;;
      *) mv "$entry" "$backup_dir/" ;;
    esac
  done

  if ! directory_can_receive_checkout "$dir"; then
    die "could not prepare $dir for checkout after backing up existing files"
  fi
  echo "Backed up existing files to $backup_dir"
}

clone_or_reuse_repo() {
  local install_dir="$1"
  local tmp_parent
  local tmp_checkout
  if is_exo_checkout "$install_dir"; then
    echo "Using existing Exo checkout at $install_dir"
    return
  fi
  mkdir -p "$install_dir"
  prepare_non_empty_install_dir "$install_dir"
  tmp_parent="$(mktemp -d)"
  tmp_checkout="$tmp_parent/exo"
  git clone --branch "$REPO_REF" "$REPO_URL" "$tmp_checkout"
  shopt -s nullglob dotglob
  for entry in "$tmp_checkout"/*; do
    local name
    name="$(basename "$entry")"
    rm -rf "$install_dir/$name"
    mv "$entry" "$install_dir/"
  done
  rmdir "$tmp_checkout" "$tmp_parent"
}

main() {
  parse_args "$@"

  echo "Exo canonical setup"
  echo "This will install Exo into the current directory, write local keys to .env, and start Exo."
  echo "Repository: $REPO_URL"
  echo "Git ref: $REPO_REF"

  check_dependencies
  ensure_docker_running

  local install_dir
  install_dir="$(choose_install_dir)"
  clone_or_reuse_repo "$install_dir"
  cd "$install_dir"
  trust_mise_config "$install_dir/mise.toml"

  local env_file="$install_dir/.env"
  if [[ ! -f "$env_file" && -f "$install_dir/.env.example" ]]; then
    cp "$install_dir/.env.example" "$env_file"
    chmod 600 "$env_file"
  else
    touch "$env_file"
    chmod 600 "$env_file"
  fi

  info "Configure API keys"
  prompt_env_secret "OPENAI_API_KEY" "$env_file" "OpenAI API key" true
  echo "Canonical setup uses WhatsApp as the default external adapter and will show a QR code to scan."

  info "Configure Exo"
  USER_NAME="$(prompt_text "Your name, or blank to skip" "$USER_NAME")"
  AGENT_NAME="$(prompt_text "Agent display name" "$AGENT_NAME")"
  local profile_file="${EXOCLAW_LOCAL_PROMPT_FILE:-$install_dir/.exo/exoclaw-profile.md}"
  write_local_profile "$profile_file" "$USER_NAME"
  echo "Wrote local Exo profile: $profile_file"

  info "Install dependencies"
  pnpm install

  info "Build exo"
  CARGO_TARGET_DIR=target cargo build -p exo --ignore-rust-version

  info "Store secrets and register model"
  ./target/debug/exo --env-file-if-exists "$env_file" secret set openai --env OPENAI_API_KEY
  ./target/debug/exo --env-file-if-exists "$env_file" model register "$MODEL_NAME" --model "$UPSTREAM_MODEL" --secret openai

  info "Start canonical Exo"
  local control_args=(fresh --canonical --agent-name "$AGENT_NAME")
  unset EXO_SETUP_ADAPTER
  export EXO_CANONICAL_PROFILE=user
  exec scripts/exo.sh "${control_args[@]}"
}

main "$@"
