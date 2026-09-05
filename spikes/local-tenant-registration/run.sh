#!/usr/bin/env bash

set -euo pipefail

readonly CENTRAL_DATABASE='reprodb_spike_central'
readonly TENANT_DATABASE='reprodb_spike_tenant_data'
readonly MYSQL_CONTAINER="${REPRODB_SPIKE_MYSQL_CONTAINER:-mysql-8}"

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repository_root=$(cd -- "$script_dir/../.." && pwd)
salt_directory="${REPRODB_SPIKE_SALT_DIR:-$(cd -- "$repository_root/../Salt" && pwd)}"
databases_created=0

mysql_in_container() {
    docker exec -i "$MYSQL_CONTAINER" \
        sh -c 'MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysql -h127.0.0.1 -uroot --default-character-set=utf8mb4 "$@"' \
        sh "$@"
}

cleanup() {
    if [[ "$databases_created" -eq 1 ]]; then
        printf '%s\n' \
            "DROP DATABASE IF EXISTS $TENANT_DATABASE;" \
            "DROP DATABASE IF EXISTS $CENTRAL_DATABASE;" \
            | mysql_in_container >/dev/null 2>&1 || true
    fi
}

trap cleanup EXIT

docker inspect "$MYSQL_CONTAINER" >/dev/null
[[ -f "$salt_directory/artisan" ]]

existing_databases=$(
    printf '%s\n' \
        'SELECT SCHEMA_NAME' \
        'FROM information_schema.SCHEMATA' \
        "WHERE SCHEMA_NAME IN ('$CENTRAL_DATABASE', '$TENANT_DATABASE');" \
        | mysql_in_container --batch --skip-column-names
)
if [[ -n "$existing_databases" ]]; then
    printf 'Refusing to overwrite existing spike databases:\n%s\n' \
        "$existing_databases" >&2
    exit 1
fi

mysql_in_container < "$script_dir/fixture.sql"
databases_created=1

safe_json=$(
    mysql_in_container --batch --skip-column-names "$CENTRAL_DATABASE" <<'SQL'
SELECT
    JSON_LENGTH(data) = 3,
    JSON_CONTAINS_PATH(data, 'all',
        '$.tenancy_db_name',
        '$.tenancy_app_color',
        '$.tenancy_enable_stock_label_control'
    ),
    JSON_SEARCH(data, 'one', '%senha%') IS NULL,
    JSON_SEARCH(data, 'one', '%password%') IS NULL
FROM tenants
WHERE id = 'reprodb_spike_tenant';
SQL
)
[[ "$safe_json" == $'1\t1\t1\t1' ]]

root_password=''
while IFS= read -r environment_entry; do
    case "$environment_entry" in
        MYSQL_ROOT_PASSWORD=*)
            root_password=${environment_entry#MYSQL_ROOT_PASSWORD=}
            break
            ;;
    esac
done < <(docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$MYSQL_CONTAINER")
[[ -n "$root_password" ]]

tenant_result=$(
    cd -- "$salt_directory"
    env \
        APP_CENTRAL_DOMAIN=localhost \
        DB_CONNECTION=mysql \
        DB_HOST=127.0.0.1 \
        DB_PORT=3306 \
        DB_DATABASE="$CENTRAL_DATABASE" \
        DB_USERNAME=root \
        DB_PASSWORD="$root_password" \
        LOG_CHANNEL=stderr \
        php artisan tinker --execute='$domain = Stancl\Tenancy\Database\Models\Domain::query()->where("domain", "reprodb-spike")->firstOrFail(); tenancy()->initialize($domain->tenant); echo tenant()->getTenantKey() . "|" . Illuminate\Support\Facades\DB::connection("tenant")->getDatabaseName() . "|" . Illuminate\Support\Facades\DB::connection("tenant")->table("reprodb_marker")->value("value") . "|" . tenant()->tenancy_app_color . "|" . (tenant()->tenancy_enable_stock_label_control ? "1" : "0"); tenancy()->end();'
)
unset root_password

expected_result='reprodb_spike_tenant|reprodb_spike_tenant_data|tenant-ready|#123456|1'
if [[ "$tenant_result" != "$expected_result" ]]; then
    printf 'Unexpected Salt tenancy result:\n%s\n' "$tenant_result" >&2
    exit 1
fi

printf '%s\n' \
    'salt_tenancy_initialization=ok' \
    'domain_resolution=ok' \
    'database_override=ok' \
    'tenant_database_query=ok' \
    'full_source_json_copied=no'
