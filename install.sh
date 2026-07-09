#!/bin/sh
set -eu

repo="${STAY_REPO:-lividsu/stay}"
version="${STAY_VERSION:-latest}"
bin_dir="${STAY_INSTALL_DIR:-$HOME/.local/bin}"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1" >&2
    exit 1
  fi
}

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    echo "Missing required command: curl or wget" >&2
    exit 1
  fi
}

finish() {
  chmod +x "$bin_dir/stay"

  echo "stay has been installed to $bin_dir/stay"
  install_completions

  case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *)
      echo
      echo "Add this to your shell config:"
      echo
      echo "export PATH=\"$bin_dir:\$PATH\""
      ;;
  esac
}

rc_has() {
  [ -f "$1" ] && grep -qF "$2" "$1" 2>/dev/null
}

# Source the bash completion from ~/.bashrc so tab completion works even when
# the bash-completion package (which auto-loads it) is not installed.
wire_bash() {
  [ "${STAY_NO_RC:-0}" = "1" ] && return 0
  rc="$HOME/.bashrc"
  rc_has "$rc" "# stay completion" && return 0
  {
    printf '\n# stay completion\n'
    printf '[ -f "%s" ] && . "%s"\n' "$1" "$1"
  } >> "$rc"
  echo "Enabled bash completion in $rc"
}

# Put the zsh completion directory on fpath and initialize the completion system.
wire_zsh() {
  [ "${STAY_NO_RC:-0}" = "1" ] && return 0
  rc="${ZDOTDIR:-$HOME}/.zshrc"
  rc_has "$rc" "# stay completion" && return 0
  {
    printf '\n# stay completion\n'
    printf 'fpath=("%s" $fpath)\n' "$1"
    printf 'autoload -Uz compinit && compinit\n'
  } >> "$rc"
  echo "Enabled zsh completion in $rc"
}

install_completions() {
  data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
  config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
  installed=""

  bash_dir="$data_home/bash-completion/completions"
  mkdir -p "$bash_dir"
  if "$bin_dir/stay" completions bash > "$bash_dir/stay" 2>/dev/null; then
    installed="$installed bash"
    wire_bash "$bash_dir/stay"
  else
    rm -f "$bash_dir/stay"
  fi

  zsh_dir="${ZDOTDIR:-$HOME}/.zsh/completions"
  mkdir -p "$zsh_dir"
  if "$bin_dir/stay" completions zsh > "$zsh_dir/_stay" 2>/dev/null; then
    installed="$installed zsh"
    wire_zsh "$zsh_dir"
  else
    rm -f "$zsh_dir/_stay"
  fi

  fish_dir="$config_home/fish/completions"
  mkdir -p "$fish_dir"
  if "$bin_dir/stay" completions fish > "$fish_dir/stay.fish" 2>/dev/null; then
    installed="$installed fish"
  else
    rm -f "$fish_dir/stay.fish"
  fi

  if [ -n "$installed" ]; then
    echo "Shell completions installed for:$installed"
    echo "Restart your shell (or open a new terminal) to use tab completion."
    echo "Skip shell config edits next time by setting STAY_NO_RC=1."
  fi
}

install_from_release() {
  os="$(uname -s)"
  if [ "$os" != "Linux" ]; then
    echo "Stay V1 only supports Linux." >&2
    return 1
  fi

  arch="$(uname -m)"
  case "$arch" in
    x86_64|amd64) target="x86_64-linux-musl" ;;
    aarch64|arm64) target="aarch64-linux-musl" ;;
    *) echo "Unsupported architecture: $arch" >&2; return 1 ;;
  esac

  if [ "$version" = "latest" ]; then
    url="https://github.com/$repo/releases/latest/download/stay-$target"
  else
    url="https://github.com/$repo/releases/download/$version/stay-$target"
  fi

  tmp="$(mktemp)"
  if download "$url" "$tmp"; then
    mkdir -p "$bin_dir"
    mv "$tmp" "$bin_dir/stay"
    return 0
  fi

  rm -f "$tmp"
  return 1
}

install_from_source() {
  need git
  need cargo

  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

  git clone --depth 1 "https://github.com/$repo.git" "$tmp_dir/stay" >/dev/null 2>&1
  cargo build --manifest-path "$tmp_dir/stay/Cargo.toml" --release

  mkdir -p "$bin_dir"
  cp "$tmp_dir/stay/target/release/stay" "$bin_dir/stay"
}

if install_from_release; then
  finish
  exit 0
fi

echo "No prebuilt release binary was found for $repo ($version)." >&2
echo "Trying to build from source on this machine..." >&2

install_from_source
finish
