# Rostwerk

An open-source ERP written in Rust: multi-tenant, API-first and AI-first.

A **core** handles authentication, tenants, data access and module hosting.
**Modules** (master data, sales, inventory, accounting, ...) plug into the core,
and each tenant can switch them on or off.

> Status: early skeleton. The core kernel, tenant isolation and a first module
> (master data / partners) work. Auth is a development token for now.

## Layout

```
crates/
  core-kernel/        TenantCtx, tenant-scoped transactions (RLS), IDs, Money, errors
  core-host/          Module trait, module registry, per-tenant module gate, tenant registry
  module-masterdata/  Business partners (customers, suppliers)
apps/
  api-server/         Binds core and modules into one HTTP service (`rostwerk` binary)
docker/               Postgres init (application role)
```

## How tenant isolation works

1. Every request is authenticated and resolved to a `TenantCtx` (tenant, actor, enabled modules).
2. Modules reach the database only through `ctx.begin(&db)`, which opens a transaction
   and sets `app.tenant_id` for that transaction.
3. Every tenant table has a Postgres row-level security policy on `tenant_id`.
   The app connects as `rostwerk_app` (no superuser, no `BYPASSRLS`), so a query
   without a tenant context sees no rows and cannot write any.
4. Routes of a module the tenant has not enabled answer `404`.

Migrations run as `rostwerk_admin`; the server itself runs as `rostwerk_app`.

## Audit log and events

- **Audit log:** every insert, update and delete on an audited table is journaled
  by the `rostwerk_audit()` trigger with actor, before and after. A module opts a
  table in with one `CREATE TRIGGER` line, so no handler can forget it.
  `audit_log` is append-only (updates, deletes, truncation and direct inserts fail)
  and tenant-scoped. Read it via `GET /v1/audit`.
- **Outbox:** modules call `tx.publish(topic, &payload)` inside their tenant
  transaction. The event is stored only if the transaction commits. A relay moves
  events to an `EventSink` (the log for now, NATS later) with at-least-once delivery.
  Topic convention: `<module>.<entity>.<event>`, e.g. `masterdata.partner.created`.

## Getting started

Requirements: Rust (stable), Docker or Podman with `docker-compose`.

```bash
docker-compose up -d
cp .env.example .env
cargo run
```

Create a tenant with the master data module, then a partner:

```bash
curl -X POST localhost:8080/admin/tenants \
  -H 'Authorization: Bearer dev-admin' -H 'content-type: application/json' \
  -d '{"name":"Demo GmbH","modules":["masterdata"]}'

curl -X POST localhost:8080/v1/partners \
  -H 'Authorization: Bearer dev:<tenant-id>:alice' -H 'content-type: application/json' \
  -d '{"name":"ACME","kind":"customer"}'
```

The OpenAPI spec is served at `GET /openapi.json`.

## Tests

```bash
cargo test
```

The integration tests in `apps/api-server/tests` need the Postgres from
`docker-compose` and the URLs from `.env`. Each test creates its own database
(`rostwerk_test_<id>`), migrates it and drops it afterwards, so tests are
independent of each other and of a running dev server.

Without database URLs the tests are skipped. In CI set `ROSTWERK_REQUIRE_DB=1`
so they fail instead.

## Configuration

| Variable | Meaning |
| --- | --- |
| `DATABASE_URL` | App connection, role subject to RLS |
| `DATABASE_ADMIN_URL` | Connection used for migrations |
| `BIND` | Listen address, default `127.0.0.1:8080` |
| `ROSTWERK_DEV_AUTH` | `1` accepts `dev:<tenant>:<user>` bearer tokens. Development only |
| `ROSTWERK_ADMIN_TOKEN` | Bearer token for `/admin`. Unset disables the admin API |

## License

Rostwerk's code is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option. You may run, host, modify and sell services around it, including commercially.

The name "Rostwerk", its logos and the look of its official user interfaces are covered
separately by the [brand licence](LICENSE-BRAND): build on the code freely, but don't
present a modified product as Rostwerk without asking.

Contributions are accepted under the same terms; see [CONTRIBUTING.md](CONTRIBUTING.md).
