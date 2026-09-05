#!/usr/bin/env bash

set -euo pipefail

readonly SMALL_LINES=5000
readonly LARGE_LINES=100000
readonly BYTES_PER_LINE=1025

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
temporary_dir=$(mktemp -d /tmp/reprodb-rust-memory.XXXXXX)
fake_bin="$temporary_dir/bin"
option_file="$temporary_dir/client.cnf"
small_dump="$temporary_dir/small.sql.zst"
large_dump="$temporary_dir/large.sql.zst"
small_time="$temporary_dir/small.time"
large_time="$temporary_dir/large.time"

cleanup() {
    [[ ! -L "$fake_bin/docker" ]] || unlink -- "$fake_bin/docker"
    [[ ! -d "$fake_bin" ]] || rmdir -- "$fake_bin"
    for path in \
        "$option_file" \
        "$small_dump.part" \
        "$small_dump" \
        "$large_dump.part" \
        "$large_dump" \
        "$small_time" \
        "$large_time"; do
        [[ ! -e "$path" ]] || unlink -- "$path"
    done
    rmdir -- "$temporary_dir"
}

trap cleanup EXIT

mkdir "$fake_bin"
ln -s "$script_dir/fixtures/fake-docker.sh" "$fake_bin/docker"
umask 077
touch "$option_file"

cargo build --quiet --release --manifest-path "$script_dir/Cargo.toml"
spike_binary="$script_dir/target/release/reprodb-streaming-spike"

measure_rss() {
    local line_count=$1
    local output=$2
    local timing=$3

    case "$(uname -s)" in
        Darwin)
            /usr/bin/time -l \
                env PATH="$fake_bin:$PATH" \
                REPRODB_SPIKE_FAKE_MODE=dump-finite \
                REPRODB_SPIKE_FAKE_LINES="$line_count" \
                "$spike_binary" dump fake-image "$option_file" tenant_one "$output" \
                >/dev/null 2>"$timing"
            awk '/maximum resident set size/ { print $1; exit }' "$timing"
            ;;
        Linux)
            /usr/bin/time -f '%M' -o "$timing" \
                env PATH="$fake_bin:$PATH" \
                REPRODB_SPIKE_FAKE_MODE=dump-finite \
                REPRODB_SPIKE_FAKE_LINES="$line_count" \
                "$spike_binary" dump fake-image "$option_file" tenant_one "$output" \
                >/dev/null 2>/dev/null
            rss_kib=$(<"$timing")
            printf '%s\n' "$((rss_kib * 1024))"
            ;;
        *)
            echo 'Memory measurement supports only macOS and Linux.' >&2
            return 1
            ;;
    esac
}

small_rss=$(measure_rss "$SMALL_LINES" "$small_dump" "$small_time")
large_rss=$(measure_rss "$LARGE_LINES" "$large_dump" "$large_time")

[[ "$small_rss" =~ ^[1-9][0-9]*$ ]]
[[ "$large_rss" =~ ^[1-9][0-9]*$ ]]

if ((large_rss > small_rss * 3)); then
    printf 'RSS grew unexpectedly: small=%s, large=%s bytes\n' \
        "$small_rss" "$large_rss" >&2
    exit 1
fi

printf '%s\n' \
    "small_stream_bytes=$((SMALL_LINES * BYTES_PER_LINE))" \
    "small_max_rss_bytes=$small_rss" \
    "large_stream_bytes=$((LARGE_LINES * BYTES_PER_LINE))" \
    "large_max_rss_bytes=$large_rss" \
    'rss_growth_limit=3x' \
    'memory_validation=ok'
