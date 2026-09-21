#!/usr/bin/env bash
# reprodb installer
#
#   curl -fsSL https://raw.githubusercontent.com/sagatechDev/reprodb/main/install.sh | bash
#
# Options (environment variables):
#   REPRODB_VERSION      tag to install (default: latest release)
#   REPRODB_INSTALL_DIR  destination directory (default: ~/.local/bin)

set -euo pipefail

REPO="${REPRODB_REPO:-sagatechDev/reprodb}"
BIN="reprodb"
VERSION="${REPRODB_VERSION:-${1:-latest}}"
INSTALL_DIR="${REPRODB_INSTALL_DIR:-$HOME/.local/bin}"

info() { printf '  %s\n' "$*"; }
ok()   { printf '\033[32m✓\033[0m %s\n' "$*"; }
die()  { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux)  os="unknown-linux-gnu" ;;
  *)      die "Unsupported OS: $(uname -s)." ;;
esac

case "$(uname -m)" in
  arm64|aarch64) arch="aarch64" ;;
  x86_64|amd64)  arch="x86_64" ;;
  *)             die "Unsupported architecture: $(uname -m)." ;;
esac

target="${arch}-${os}"
if [ "$target" = "aarch64-unknown-linux-gnu" ]; then
  die "No prebuilt binary for Linux arm64 yet."
fi

asset="${BIN}-${target}.tar.gz"
if [ "$VERSION" = "latest" ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

info "Platform: $target"
info "Version:  $VERSION"

fetch() {
  # $1 = file name; downloads into $tmp, transparently handling a private repo.
  if curl -fsSL -o "$tmp/$1" "$base/$1" 2>/dev/null; then
    return 0
  fi
  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    local args=(--repo "$REPO" --dir "$tmp" --pattern "$1" --clobber)
    [ "$VERSION" != "latest" ] && args=("$VERSION" "${args[@]}")
    gh release download "${args[@]}" >/dev/null 2>&1 && return 0
  fi
  return 1
}

command -v curl >/dev/null 2>&1 || die "curl is required."
fetch "$asset" || die "Could not download $asset from $REPO. Check https://github.com/$REPO/releases"
fetch "SHA256SUMS" || true

if [ -f "$tmp/SHA256SUMS" ]; then
  if command -v sha256sum >/dev/null 2>&1; then
    sum="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
  else
    sum="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
  fi
  grep -q "$sum" "$tmp/SHA256SUMS" || die "Checksum mismatch for $asset."
  ok "Checksum verified"
fi

tar -xzf "$tmp/$asset" -C "$tmp"
[ -f "$tmp/$BIN" ] || die "Binary '$BIN' not found inside the archive."

mkdir -p "$INSTALL_DIR"
install -m 755 "$tmp/$BIN" "$INSTALL_DIR/$BIN"
ok "Installed $INSTALL_DIR/$BIN"

"$INSTALL_DIR/$BIN" --version || true

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    printf '\n\033[33m!\033[0m %s is not in your PATH. Add it:\n\n' "$INSTALL_DIR"
    printf '  bash/zsh: echo '\''export PATH="%s:$PATH"'\'' >> ~/.zshrc\n' "$INSTALL_DIR"
    printf '  fish:     fish_add_path %s\n\n' "$INSTALL_DIR"
    ;;
esac

printf '\nNext: %s setup && %s doctor\n' "$BIN" "$BIN"
