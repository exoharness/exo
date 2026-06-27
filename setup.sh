#!/usr/bin/env bash
set -euo pipefail

REPO_URL="${EXO_REPO_URL:-https://github.com/ankrgyl/exo.git}"
REPO_REF="${EXO_REPO_REF:-main}"
INSTALL_DIR="${EXO_INSTALL_DIR:-$PWD}"
MODEL_NAME="${EXO_MODEL:-gpt-5.4}"
UPSTREAM_MODEL="${EXO_UPSTREAM_MODEL:-$MODEL_NAME}"
AGENT_NAME="${EXO_AGENT_NAME:-ExoClaw}"
USER_NAME="${EXO_USER_NAME:-}"

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
  --help                Show this help

Environment overrides:
  EXO_REPO_URL, EXO_REPO_REF, EXO_INSTALL_DIR, EXO_MODEL, EXO_UPSTREAM_MODEL,
  EXO_AGENT_NAME, EXO_USER_NAME, EXOCLAW_LOCAL_PROMPT_FILE
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
    missing+=("git: install Git")
  command -v node >/dev/null 2>&1 ||
    missing+=("node: install Node.js 22+")
  command -v pnpm >/dev/null 2>&1 ||
    missing+=("pnpm: install pnpm (https://pnpm.io/installation)")
  command -v cargo >/dev/null 2>&1 ||
    missing+=("cargo: install Rust with rustup (https://rustup.rs/)")
  command -v rustc >/dev/null 2>&1 ||
    missing+=("rustc: install Rust with rustup (https://rustup.rs/)")
  command -v docker >/dev/null 2>&1 ||
    missing+=("docker: install Docker Desktop (https://www.docker.com/products/docker-desktop/)")

  if ((${#missing[@]} > 0)); then
    echo "Missing required dependencies:" >&2
    local item
    for item in "${missing[@]}"; do
      echo "  - $item" >&2
    done
    exit 1
  fi
}

is_exo_checkout() {
  local dir="$1"
  [[ -d "$dir/.git" && -f "$dir/examples/exoclaw/scripts/exoclaw-control" ]]
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
  local value
  existing="$(env_value "$key" "$file")"
  if [[ -n "$existing" ]]; then
    if prompt_yes_no "$key is already set in .env. Keep it?" y; then
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
    echo "# Local Exoclaw Profile"
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

clone_or_reuse_repo() {
  local install_dir="$1"
  local tmp_parent
  local tmp_checkout
  if is_exo_checkout "$install_dir"; then
    echo "Using existing Exo checkout at $install_dir"
    return
  fi
  mkdir -p "$install_dir"
  if ! directory_can_receive_checkout "$install_dir"; then
    die "$install_dir is not empty. Run setup from an empty directory or set EXO_INSTALL_DIR."
  fi
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
  echo "This will install Exo into the current directory, write local keys to .env, and start Exoclaw."
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

  info "Configure Exoclaw"
  USER_NAME="$(prompt_text "Your name, or blank to skip" "$USER_NAME")"
  AGENT_NAME="$(prompt_text "Agent display name" "$AGENT_NAME")"
  local profile_file="${EXOCLAW_LOCAL_PROMPT_FILE:-$install_dir/.exo/exoclaw-profile.md}"
  write_local_profile "$profile_file" "$USER_NAME"
  echo "Wrote local Exoclaw profile: $profile_file"

  info "Install dependencies"
  pnpm install

  info "Build exo"
  CARGO_TARGET_DIR=target cargo build -p exo --ignore-rust-version

  info "Store secrets and register model"
  ./target/debug/exo --env-file-if-exists "$env_file" secret set openai --env OPENAI_API_KEY
  ./target/debug/exo --env-file-if-exists "$env_file" model register "$MODEL_NAME" --model "$UPSTREAM_MODEL" --secret openai

  info "Start canonical Exoclaw"
  local control_args=(fresh --canonical --agent-name "$AGENT_NAME")
  unset EXO_SETUP_ADAPTER
  export EXO_CANONICAL_PROFILE=user
  exec examples/exoclaw/scripts/exoclaw-control "${control_args[@]}"
}

main "$@"
