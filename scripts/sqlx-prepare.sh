#!/usr/bin/env bash
# Regenerates .sqlx/ (the offline query data for sqlx::query! macros) against a
# scratch database migrated from crates/*/migrations. Pass --check to only verify
# that .sqlx/ is up to date (CI does this).
#
# Needs sqlx-cli 0.8 (`cargo install sqlx-cli --version '^0.8' --no-default-features
# --features postgres,rustls --locked`) and DATABASE_ADMIN_URL (environment or .env),
# a role that may create databases. The scratch database rostwerk_sqlx on the same
# server is dropped and recreated each run.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ -z "${DATABASE_ADMIN_URL:-}" && -f .env ]]; then
    DATABASE_ADMIN_URL=$(sed -n 's/^DATABASE_ADMIN_URL=//p' .env | tail -n 1)
fi
: "${DATABASE_ADMIN_URL:?DATABASE_ADMIN_URL is not set (see .env.example)}"

# Same server and role, other database name. Query parameters are kept.
base=${DATABASE_ADMIN_URL%%\?*}
query=${DATABASE_ADMIN_URL#"$base"}
export DATABASE_URL="${base%/*}/rostwerk_sqlx${query}"

sqlx database drop -y >/dev/null
sqlx database create

# Same order and history handling as Registry::migrate: core first, then the
# modules; one shared history table, each directory ignores the others' entries.
dirs=(crates/core-host/migrations)
for dir in crates/*/migrations; do
    [[ $dir == crates/core-host/migrations ]] || dirs+=("$dir")
done
for dir in "${dirs[@]}"; do
    sqlx migrate run --source "$dir" --ignore-missing
done

cargo sqlx prepare --workspace "$@" -- --all-targets
