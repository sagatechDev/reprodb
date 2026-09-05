#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo build --quiet
reprodb_bin="$repo_root/target/debug/reprodb"

run_preview() {
  local title="$1"
  shift

  printf '\n\033[1;36m%s\033[0m\n\n' "$title"
  "$reprodb_bin" "$@"
}

run_preview "1/6 · Command discovery" --help
run_preview "2/6 · Profile command discovery" profile --help
run_preview "3/6 · Local Docker target" setup --preview
run_preview "4/6 · Source profile" profile add salt-local --preview
run_preview "5/6 · Environment diagnosis" doctor --preview
run_preview "6/6 · Main tenant flow" pull guerra --preview

printf '\n\033[1;32mPreview completed. No reprodb state was changed.\033[0m\n'
