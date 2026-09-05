#!/usr/bin/env bash

set -euo pipefail

readonly SOURCE_DATABASE='reprodb_spike_source'
readonly TARGET_DATABASE='reprodb_spike_target'
readonly MYSQL_CONTAINER="${REPRODB_SPIKE_MYSQL_CONTAINER:-mysql-8}"
readonly CLIENT_IMAGE="${REPRODB_SPIKE_CLIENT_IMAGE:-mysql@sha256:1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c}"

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
temporary_dir=$(mktemp -d /tmp/reprodb-rust-spike.XXXXXX)
option_file="$temporary_dir/client.cnf"
dump_file="$temporary_dir/fixture.sql.zst"
databases_created=0

mysql_in_server_container() {
    docker exec -i "$MYSQL_CONTAINER" \
        sh -c 'MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysql -uroot --default-character-set=utf8mb4 "$@"' \
        sh "$@"
}

cleanup() {
    if [[ "$databases_created" -eq 1 ]]; then
        printf '%s\n' \
            "DROP DATABASE IF EXISTS $TARGET_DATABASE;" \
            "DROP DATABASE IF EXISTS $SOURCE_DATABASE;" \
            | mysql_in_server_container >/dev/null 2>&1 || true
    fi

    [[ ! -e "$dump_file.part" ]] || unlink -- "$dump_file.part"
    [[ ! -e "$dump_file" ]] || unlink -- "$dump_file"
    [[ ! -e "$option_file" ]] || unlink -- "$option_file"
    rmdir -- "$temporary_dir"
}

trap cleanup EXIT

docker inspect "$MYSQL_CONTAINER" >/dev/null

existing_databases=$(
    printf '%s\n' \
        "SELECT SCHEMA_NAME" \
        "FROM information_schema.SCHEMATA" \
        "WHERE SCHEMA_NAME IN ('$SOURCE_DATABASE', '$TARGET_DATABASE');" \
        | mysql_in_server_container --batch --skip-column-names
)

if [[ -n "$existing_databases" ]]; then
    printf 'Refusing to overwrite existing spike databases:\n%s\n' "$existing_databases" >&2
    exit 1
fi

root_password=''
while IFS= read -r environment_entry; do
    case "$environment_entry" in
        MYSQL_ROOT_PASSWORD=*)
            root_password=${environment_entry#MYSQL_ROOT_PASSWORD=}
            break
            ;;
    esac
done < <(docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$MYSQL_CONTAINER")

if [[ -z "$root_password" ]]; then
    echo 'The local spike container does not expose MYSQL_ROOT_PASSWORD.' >&2
    exit 1
fi

umask 077
{
    printf '%s\n' \
        '[client]' \
        'host=host.docker.internal' \
        'port=3306' \
        'user=root' \
        'protocol=TCP'
    printf 'password="%s"\n' "$root_password"
} > "$option_file"
unset root_password
chmod 600 "$option_file"

mysql_in_server_container < "$script_dir/fixtures/setup.sql"
databases_created=1

cargo build --quiet --manifest-path "$script_dir/Cargo.toml"
spike_binary="$script_dir/target/debug/reprodb-streaming-spike"

"$spike_binary" \
    dump \
    "$CLIENT_IMAGE" \
    "$option_file" \
    "$SOURCE_DATABASE" \
    "$dump_file"

[[ -f "$dump_file" ]]
[[ ! -e "$dump_file.part" ]]

"$spike_binary" \
    restore \
    "$CLIENT_IMAGE" \
    "$option_file" \
    "$TARGET_DATABASE" \
    "$dump_file"

validation=$(
    mysql_in_server_container --batch --skip-column-names \
        < "$script_dir/fixtures/validate.sql"
)

expected_validation=$'1\t1\t1\t1\t1\t1\n2\t10000\n1\n1\nutf8mb4_unicode_ci'
if [[ "$validation" != "$expected_validation" ]]; then
    printf 'Unexpected validation output:\n%s\n' "$validation" >&2
    exit 1
fi

compressed_bytes=$(wc -c < "$dump_file" | tr -d ' ')
if docker ps -a --format '{{.Names}}' | grep -Eq '^reprodb-spike-'; then
    echo 'A spike client container was left behind.' >&2
    exit 1
fi

printf 'validation=ok\ncompressed_bytes=%s\norphan_client_container=no\n' "$compressed_bytes"
