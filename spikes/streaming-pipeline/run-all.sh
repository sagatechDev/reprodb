#!/usr/bin/env bash

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

"$script_dir/run-ephemeral-e2e.sh"
"$script_dir/run-cancellation-test.sh"
"$script_dir/run-memory-test.sh"
