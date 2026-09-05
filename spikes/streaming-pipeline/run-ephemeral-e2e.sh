#!/usr/bin/env bash

set -euo pipefail

readonly SOURCE_DATABASE='reprodb_spike_source'
readonly TARGET_DATABASE='reprodb_spike_target'
readonly TEST_PASSWORD='reprodb-spike-local-only'
readonly SERVER_IMAGE="${REPRODB_SPIKE_SERVER_IMAGE:-mysql@sha256:1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c}"
readonly CLIENT_IMAGE="${REPRODB_SPIKE_CLIENT_IMAGE:-$SERVER_IMAGE}"

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
temporary_dir=$(mktemp -d /tmp/reprodb-rust-ephemeral.XXXXXX)
run_id="$(basename "$temporary_dir")-$$"
source_container="reprodb-spike-source-$run_id"
target_container="reprodb-spike-target-$run_id"
source_option_file="$temporary_dir/source.cnf"
target_option_file="$temporary_dir/target.cnf"
dump_file="$temporary_dir/fixture.sql.zst"
source_created=0
target_created=0

cleanup() {
    if [[ "$target_created" -eq 1 ]]; then
        docker rm --force "$target_container" >/dev/null 2>&1 || true
    fi
    if [[ "$source_created" -eq 1 ]]; then
        docker rm --force "$source_container" >/dev/null 2>&1 || true
    fi

    [[ ! -e "$dump_file.part" ]] || unlink -- "$dump_file.part"
    [[ ! -e "$dump_file" ]] || unlink -- "$dump_file"
    [[ ! -e "$source_option_file" ]] || unlink -- "$source_option_file"
    [[ ! -e "$target_option_file" ]] || unlink -- "$target_option_file"
    rmdir -- "$temporary_dir"
}

trap cleanup EXIT

create_server() {
    local container=$1

    docker create \
        --name "$container" \
        --label "reprodb.spike.run=$run_id" \
        --env "MYSQL_ROOT_PASSWORD=$TEST_PASSWORD" \
        --publish 127.0.0.1::3306 \
        "$SERVER_IMAGE" \
        --character-set-server=utf8mb4 \
        --collation-server=utf8mb4_unicode_ci \
        >/dev/null
}

wait_for_mysql() {
    local container=$1
    local attempt

    for attempt in {1..60}; do
        if docker exec "$container" \
            sh -c 'MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysqladmin ping -h127.0.0.1 -uroot --silent' \
            >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done

    echo "MySQL did not become ready in container $container." >&2
    docker logs "$container" >&2 || true
    return 1
}

mysql_in_server() {
    local container=$1
    shift

    docker exec -i "$container" \
        sh -c 'MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysql -h127.0.0.1 -uroot --default-character-set=utf8mb4 "$@"' \
        sh "$@"
}

published_port() {
    local container=$1
    local endpoint

    endpoint=$(docker port "$container" 3306/tcp)
    printf '%s\n' "${endpoint##*:}"
}

write_option_file() {
    local path=$1
    local port=$2

    {
        printf '%s\n' \
            '[client]' \
            'host=host.docker.internal' \
            "port=$port" \
            'user=root' \
            "password=$TEST_PASSWORD" \
            'protocol=TCP'
    } > "$path"
    chmod 600 "$path"
}

docker info >/dev/null
umask 077

create_server "$source_container"
source_created=1
docker start "$source_container" >/dev/null

create_server "$target_container"
target_created=1
docker start "$target_container" >/dev/null

wait_for_mysql "$source_container"
wait_for_mysql "$target_container"

source_port=$(published_port "$source_container")
target_port=$(published_port "$target_container")
write_option_file "$source_option_file" "$source_port"
write_option_file "$target_option_file" "$target_port"

mysql_in_server "$source_container" < "$script_dir/fixtures/setup-source.sql"
mysql_in_server "$target_container" < "$script_dir/fixtures/setup-target.sql"

cargo build --quiet --manifest-path "$script_dir/Cargo.toml"
spike_binary="$script_dir/target/debug/reprodb-streaming-spike"

"$spike_binary" \
    dump \
    "$CLIENT_IMAGE" \
    "$source_option_file" \
    "$SOURCE_DATABASE" \
    "$dump_file"

[[ -f "$dump_file" ]]
[[ ! -e "$dump_file.part" ]]

"$spike_binary" \
    restore \
    "$CLIENT_IMAGE" \
    "$target_option_file" \
    "$TARGET_DATABASE" \
    "$dump_file"

source_snapshot=$(
    mysql_in_server "$source_container" --batch --skip-column-names "$SOURCE_DATABASE" \
        < "$script_dir/fixtures/snapshot.sql"
)
target_snapshot=$(
    mysql_in_server "$target_container" --batch --skip-column-names "$TARGET_DATABASE" \
        < "$script_dir/fixtures/snapshot.sql"
)

if [[ "$source_snapshot" != "$target_snapshot" ]]; then
    printf 'Source and target snapshots differ.\nSource:\n%s\nTarget:\n%s\n' \
        "$source_snapshot" "$target_snapshot" >&2
    exit 1
fi

target_assertions=$(
    mysql_in_server "$target_container" --batch --skip-column-names "$TARGET_DATABASE" \
        < "$script_dir/fixtures/assert-target.sql"
)
if [[ "$target_assertions" != $'1\t1\t1\t1\t1\t1\t1' ]]; then
    printf 'Target invariants failed: %s\n' "$target_assertions" >&2
    exit 1
fi

compressed_bytes=$(wc -c < "$dump_file" | tr -d ' ')
printf '%s\n' \
    'validation=ok' \
    'source_target_servers=independent' \
    "compressed_bytes=$compressed_bytes"
