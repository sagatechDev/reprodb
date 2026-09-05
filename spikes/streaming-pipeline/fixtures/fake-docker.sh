#!/usr/bin/env bash

set -euo pipefail

if [[ "${1:-}" == 'kill' ]]; then
    exit 0
fi

if [[ "${1:-}" != 'run' ]]; then
    printf 'unexpected fake docker operation: %s\n' "${1:-<none>}" >&2
    exit 2
fi

client=''
for argument in "$@"; do
    case "$argument" in
        mysqldump|mysql)
            client=$argument
            ;;
    esac
done

readonly chunk='0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'

case "${REPRODB_SPIKE_FAKE_MODE:-}" in
    dump-slow)
        [[ "$client" == 'mysqldump' ]]
        while :; do
            printf '%s%s%s%s%s%s%s%s\n' \
                "$chunk" "$chunk" "$chunk" "$chunk" \
                "$chunk" "$chunk" "$chunk" "$chunk"
        done
        ;;
    dump-finite)
        [[ "$client" == 'mysqldump' ]]
        line_count=${REPRODB_SPIKE_FAKE_LINES:-50000}
        [[ "$line_count" =~ ^[1-9][0-9]*$ ]]
        for ((line = 0; line < line_count; line++)); do
            printf '%s%s%s%s%s%s%s%s\n' \
                "$chunk" "$chunk" "$chunk" "$chunk" \
                "$chunk" "$chunk" "$chunk" "$chunk"
        done
        ;;
    restore-stall)
        [[ "$client" == 'mysql' ]]
        while :; do
            sleep 1
        done
        ;;
    *)
        printf 'unexpected REPRODB_SPIKE_FAKE_MODE: %s\n' \
            "${REPRODB_SPIKE_FAKE_MODE:-<none>}" >&2
        exit 2
        ;;
esac
