#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This smoke suite must run on macOS." >&2
  exit 2
fi

architecture="$(uname -m)"
case "$architecture" in
  arm64 | x86_64) ;;
  *)
    echo "Unsupported macOS architecture: $architecture" >&2
    exit 2
    ;;
esac

echo "reprodb macOS smoke · $architecture"
echo "Local fixture: ${REPRODB_TEST_MYSQL_CONTAINER:-mysql-8}"
echo

docker info >/dev/null
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features

# Creates a random native credential and removes it before returning.
cargo test --lib \
  infrastructure::credentials::store::tests::native_store_roundtrips_a_temporary_credential \
  -- --ignored --exact --nocapture

# Read-only checks against the developer's local MySQL fixture.
cargo test --test docker_discovery_integration -- --ignored --nocapture
cargo test --test docker_client_integration -- --ignored --nocapture

# Self-contained source A -> target B scenario using the actual CLI binary.
cargo test --features test-file-credential-store \
  --test pull_integration \
  real_cli_configures_pulls_and_reuses_cache_with_the_source_offline \
  -- --ignored --nocapture

echo
echo "macOS $architecture smoke passed."
