#!/usr/bin/env bash
# Runs the api-server for the spike against its own database (rostwerk_ui_spike
# on the docker-compose Postgres), so seeded partners stay out of the dev database.
# Listens on 127.0.0.1:18080; the Dioxus dev server proxies /v1 there.
set -euo pipefail
cd "$(dirname "$0")/../../.."

admin=$(sed -n 's/^DATABASE_ADMIN_URL=//p' .env | tail -n 1)
app=$(sed -n 's/^DATABASE_URL=//p' .env | tail -n 1)
db=rostwerk_ui_spike
admin_db="${admin%/*}/$db"
app_db="${app%/*}/$db"
app_role=$(sed -E 's#^postgres(ql)?://([^:@]+).*#\2#' <<<"$app")
admin_role=$(sed -E 's#^postgres(ql)?://([^:@]+).*#\2#' <<<"$admin")

if ! psql "$admin" -tAc "SELECT 1 FROM pg_database WHERE datname = '$db'" | grep -q 1; then
    psql "$admin" -qc "CREATE DATABASE $db"
    psql "$admin_db" -q <<SQL
GRANT CONNECT ON DATABASE $db TO "$app_role";
GRANT USAGE ON SCHEMA public TO "$app_role";
ALTER DEFAULT PRIVILEGES FOR ROLE "$admin_role" IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO "$app_role";
SQL
fi

export DATABASE_URL="$app_db" DATABASE_ADMIN_URL="$admin_db" BIND=127.0.0.1:18080
exec cargo run -q -p api-server --bin rostwerk
