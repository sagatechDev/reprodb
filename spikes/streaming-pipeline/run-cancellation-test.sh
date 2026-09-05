#!/usr/bin/env bash

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
temporary_dir=$(mktemp -d /tmp/reprodb-rust-cancellation.XXXXXX)
fake_bin="$temporary_dir/bin"
option_file="$temporary_dir/client.cnf"
dump_file="$temporary_dir/fixture.sql.zst"
dump_stderr="$temporary_dir/dump.stderr"
restore_stderr="$temporary_dir/restore.stderr"
active_pid=''

cleanup() {
    if [[ -n "$active_pid" ]]; then
        kill -KILL "$active_pid" >/dev/null 2>&1 || true
        wait "$active_pid" >/dev/null 2>&1 || true
    fi

    [[ ! -L "$fake_bin/docker" ]] || unlink -- "$fake_bin/docker"
    [[ ! -d "$fake_bin" ]] || rmdir -- "$fake_bin"
    for path in \
        "$dump_file.part" \
        "$dump_file" \
        "$option_file" \
        "$dump_stderr" \
        "$restore_stderr"; do
        [[ ! -e "$path" ]] || unlink -- "$path"
    done
    rmdir -- "$temporary_dir"
}

trap cleanup EXIT

mkdir "$fake_bin"
ln -s "$script_dir/fixtures/fake-docker.sh" "$fake_bin/docker"
umask 077
printf '%s\n' '[client]' > "$option_file"

cargo build --quiet --manifest-path "$script_dir/Cargo.toml"
spike_binary="$script_dir/target/debug/reprodb-streaming-spike"

PATH="$fake_bin:$PATH" REPRODB_SPIKE_FAKE_MODE=dump-slow \
    "$spike_binary" dump fake-image "$option_file" tenant_one "$dump_file" \
    >/dev/null 2>"$dump_stderr" &
active_pid=$!

for _ in {1..100}; do
    [[ ! -e "$dump_file.part" ]] || break
    sleep 0.02
done
[[ -e "$dump_file.part" ]]

kill -INT "$active_pid"
set +e
wait "$active_pid"
dump_status=$?
set -e
active_pid=''

[[ "$dump_status" -eq 130 ]]
[[ ! -e "$dump_file.part" ]]
[[ ! -e "$dump_file" ]]
grep -Fq 'operation interrupted' "$dump_stderr"

PATH="$fake_bin:$PATH" REPRODB_SPIKE_FAKE_MODE=dump-finite \
    "$spike_binary" dump fake-image "$option_file" tenant_one "$dump_file" \
    >/dev/null 2>/dev/null
[[ -s "$dump_file" ]]

PATH="$fake_bin:$PATH" REPRODB_SPIKE_FAKE_MODE=restore-stall \
    "$spike_binary" restore fake-image "$option_file" tenant_two "$dump_file" \
    >/dev/null 2>"$restore_stderr" &
active_pid=$!
sleep 0.2

kill -INT "$active_pid"
set +e
wait "$active_pid"
restore_status=$?
set -e
active_pid=''

[[ "$restore_status" -eq 130 ]]
[[ -s "$dump_file" ]]
grep -Fq 'operation interrupted' "$restore_stderr"

printf '%s\n' \
    'dump_cancellation_exit=130' \
    'dump_partial_removed=yes' \
    'restore_cancellation_exit=130' \
    'completed_dump_preserved=yes'
