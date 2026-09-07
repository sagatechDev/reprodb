#!/usr/bin/env bash
set -euo pipefail

case "$(uname -s)" in
  Darwin)
    measure=(/usr/bin/time -l)
    memory_note="maximum resident set size is reported in bytes"
    ;;
  Linux)
    measure=(/usr/bin/time -v)
    memory_note="maximum resident set size is reported in KiB"
    ;;
  *)
    echo "This benchmark supports macOS and Linux." >&2
    exit 2
    ;;
esac

large_rows="${REPRODB_BENCHMARK_LARGE_ROWS:-450000}"
test_name="pulls_a_real_tenant_then_reuses_cache_without_the_source_credential"

echo "reprodb local benchmark"
echo "Large fixture rows: $large_rows"
echo "Memory note: $memory_note"
echo

docker info >/dev/null
cargo test --release --test pull_integration --no-run

run_case() {
  local rows="$1"
  local level="$2"

  echo
  echo "rows=$rows zstd_level=$level"
  REPRODB_BENCHMARK_ROWS="$rows" \
    REPRODB_BENCHMARK_ZSTD_LEVEL="$level" \
    "${measure[@]}" cargo test --release --test pull_integration "$test_name" \
      -- --ignored --exact --nocapture
}

run_case 2 1
run_case "$large_rows" 1
run_case "$large_rows" 3
