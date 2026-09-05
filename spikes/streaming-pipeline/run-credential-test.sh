#!/usr/bin/env bash

set -euo pipefail

readonly TEST_ROOT_PASSWORD='reprodb-spike-root-local-only'
readonly SECRET_MARKER='credential-secret-marker'
readonly SERVER_IMAGE="${REPRODB_SPIKE_SERVER_IMAGE:-mysql@sha256:1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c}"
readonly CLIENT_IMAGE="${REPRODB_SPIKE_CLIENT_IMAGE:-$SERVER_IMAGE}"

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
temporary_dir=$(mktemp -d /tmp/reprodb-rust-credential.XXXXXX)
run_id="$(basename "$temporary_dir")-$$"
server_container="reprodb-spike-credential-server-$run_id"
client_container="reprodb-spike-credential-client-$run_id"
option_file="$temporary_dir/client.cnf"
wrong_option_file="$temporary_dir/wrong-client.cnf"
helper_stderr="$temporary_dir/helper.stderr"
client_stderr="$temporary_dir/client.stderr"
server_created=0
client_created=0

cleanup() {
    if [[ "$client_created" -eq 1 ]]; then
        docker rm --force "$client_container" >/dev/null 2>&1 || true
    fi
    if [[ "$server_created" -eq 1 ]]; then
        docker rm --force "$server_container" >/dev/null 2>&1 || true
    fi

    for path in \
        "$option_file" \
        "$wrong_option_file" \
        "$helper_stderr" \
        "$client_stderr"; do
        [[ ! -e "$path" ]] || unlink -- "$path"
    done
    rmdir -- "$temporary_dir"
}

trap cleanup EXIT

docker create \
    --name "$server_container" \
    --label "reprodb.spike.run=$run_id" \
    --env "MYSQL_ROOT_PASSWORD=$TEST_ROOT_PASSWORD" \
    --publish 127.0.0.1::3306 \
    "$SERVER_IMAGE" \
    >/dev/null
server_created=1
docker start "$server_container" >/dev/null

for _ in {1..60}; do
    if docker exec "$server_container" \
        sh -c 'MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysqladmin ping -h127.0.0.1 -uroot --silent' \
        >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

docker exec "$server_container" \
    sh -c 'MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysqladmin ping -h127.0.0.1 -uroot --silent' \
    >/dev/null

docker exec -i "$server_container" \
    sh -c 'MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysql -h127.0.0.1 -uroot' \
    < "$script_dir/fixtures/setup-credential-user.sql"

server_endpoint=$(docker port "$server_container" 3306/tcp)
server_port=${server_endpoint##*:}

cargo build --quiet --manifest-path "$script_dir/Cargo.toml" --bin option-file-spike
option_file_helper="$script_dir/target/debug/option-file-spike"
test_password=$' credential-secret-marker \'"#;\\\n\t\r trailing '

printf '%s' "$test_password" \
    | "$option_file_helper" \
        "$option_file" \
        host.docker.internal \
        "$server_port" \
        credential_spike \
        2>"$helper_stderr"

[[ ! -s "$helper_stderr" ]]
case "$(uname -s)" in
    Darwin) option_mode=$(stat -f '%Lp' "$option_file") ;;
    Linux) option_mode=$(stat -c '%a' "$option_file") ;;
    *) echo 'Permission validation supports only macOS and Linux.' >&2; exit 1 ;;
esac
[[ "$option_mode" == '600' ]]

docker create \
    --name "$client_container" \
    --label "reprodb.spike.run=$run_id" \
    --add-host=host.docker.internal:host-gateway \
    --mount "type=bind,src=$option_file,dst=/run/secrets/reprodb.cnf,readonly" \
    "$CLIENT_IMAGE" \
    mysql \
    --defaults-file=/run/secrets/reprodb.cnf \
    --no-login-paths \
    --batch \
    --skip-column-names \
    --execute 'SELECT CURRENT_USER()' \
    >/dev/null
client_created=1

client_inspect=$(docker inspect "$client_container")
if [[ "$client_inspect" == *"$SECRET_MARKER"* ]]; then
    echo 'Secret marker leaked into the client container configuration.' >&2
    exit 1
fi

mount_writable=$(
    docker inspect \
        --format '{{range .Mounts}}{{if eq .Destination "/run/secrets/reprodb.cnf"}}{{.RW}}{{end}}{{end}}' \
        "$client_container"
)
[[ "$mount_writable" == 'false' ]]

current_user=$(docker start --attach "$client_container")
[[ "$current_user" == 'credential_spike@%' ]]

printf '%s' "wrong-$test_password" \
    | "$option_file_helper" \
        "$wrong_option_file" \
        host.docker.internal \
        "$server_port" \
        credential_spike \
        2>/dev/null

set +e
docker run --rm \
    --add-host=host.docker.internal:host-gateway \
    --mount "type=bind,src=$wrong_option_file,dst=/run/secrets/reprodb.cnf,readonly" \
    "$CLIENT_IMAGE" \
    mysql \
    --defaults-file=/run/secrets/reprodb.cnf \
    --no-login-paths \
    --execute 'SELECT 1' \
    >/dev/null 2>"$client_stderr"
wrong_password_status=$?
set -e

[[ "$wrong_password_status" -ne 0 ]]
if grep -Fq "$SECRET_MARKER" "$client_stderr"; then
    echo 'Secret marker leaked into the MySQL client error.' >&2
    exit 1
fi

printf '%s\n' \
    'special_character_authentication=ok' \
    'option_file_mode=600' \
    'option_file_mount_read_only=yes' \
    'secret_in_container_config=no' \
    'secret_in_authentication_error=no'
