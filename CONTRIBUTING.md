# Contributing to Rostwerk

Thanks for helping build Rostwerk.

## License of contributions

There is no CLA to sign. Unless you explicitly state otherwise, any contribution you
intentionally submit for inclusion in Rostwerk, as defined in the Apache-2.0 license,
is dual licensed under MIT OR Apache-2.0, without any additional terms or conditions.

Only submit work you have the right to contribute. Contributions to files listed as
Brand Material in [LICENSE-BRAND](LICENSE-BRAND) (such as `brand/`) are accepted under
that licence's terms.

## Finding work

Open work is tracked as GitHub issues. Take one labeled `ready`; [AGENTS.md](AGENTS.md)
describes how to claim it, name the branch and open the PR. It applies to people and
coding agents alike.

## Development setup

See [README.md](README.md#getting-started): `docker-compose up -d`, `cp .env.example .env`,
`cargo run`.

## Before opening a pull request

CI runs these, so run them locally first:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The integration tests need the Postgres from `docker-compose`. Each test creates and drops
its own database.

## SQL queries

Queries use the compile-time checked macros `sqlx::query!` and `sqlx::query_as!`, so a
query that does not match the schema fails the build. Integration tests in
`apps/api-server/tests` may use the runtime functions.

The macros read the query metadata from `.sqlx/` (committed), not from a database:
`.cargo/config.toml` sets `SQLX_OFFLINE=true`, so building needs no Postgres.

Regenerate `.sqlx/` and commit it whenever you add or change a query or a migration:

```bash
cargo install sqlx-cli --version '^0.8' --no-default-features --features postgres,rustls --locked
scripts/sqlx-prepare.sh
```

The script uses `DATABASE_ADMIN_URL` (from `.env`), recreates the scratch database
`rostwerk_sqlx` on that server, applies all migrations and runs
`cargo sqlx prepare --workspace`. CI runs `scripts/sqlx-prepare.sh --check` and fails
if `.sqlx/` is stale. A build error like "no cached data for this query" means the
same thing: run the script.

For an enum stored as `text`, name the Rust type in the column alias
(`kind AS "kind: PartnerKind"`) and cast the argument (`kind as PartnerKind`).

## Ground rules for code

- Modules never read another module's tables. They talk through the core: queries,
  commands, events.
- Tenant data is reached only through `TenantCtx::begin`. Every tenant table has a
  row-level security policy on `tenant_id` and, if it holds business data, the
  `rostwerk_audit()` trigger.
- Events go through `TenantTx::publish` in the same transaction as the change.
- Money is `rust_decimal`, never floats.
