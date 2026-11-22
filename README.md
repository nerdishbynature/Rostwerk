# Rostwerk

An open-source ERP written in Rust: multi-tenant, API-first and AI-first.

A **core** handles authentication, tenants, data access and module hosting.
**Modules** (master data, sales, inventory, accounting, ...) plug into the core,
and each tenant can switch them on or off.

> Status: early skeleton. The core kernel, tenant isolation, OIDC authentication
> and a first module (master data / partners) work.

## Layout

```
crates/
  core-kernel/        TenantCtx, tenant-scoped transactions (RLS), IDs, Money, errors
  core-host/          Module trait, module registry, per-tenant module gate, tenant registry
  module-masterdata/  Business partners (customers, suppliers) and articles
  module-accounting/  Chart of accounts, double-entry journal with immutable posted entries
  event-nats/         Event sink that publishes outbox events to NATS JetStream
apps/
  api-server/         Binds core and modules into one HTTP service (`rostwerk` binary)
docker/               Postgres init (application role), Keycloak dev realm
```

How the pieces fit together (crates, request lifecycle, tenant isolation with
row-level security, audit log, outbox, module boundaries) and what is planned is
described in [docs/architecture.md](docs/architecture.md).

In short: every request is resolved to a tenant, modules reach the database only
through a tenant-scoped transaction with Postgres row-level security, and routes of a
module the tenant has not enabled answer `404`. Migrations run as `rostwerk_admin`;
the server itself runs as `rostwerk_app` (no superuser, no `BYPASSRLS`).

## Authentication

The API accepts two kinds of bearer tokens:

- **OIDC access tokens** (JWT) from the issuer in `ROSTWERK_OIDC_ISSUER`. Rostwerk
  finds the signing keys via `<issuer>/.well-known/openid-configuration`, caches the
  JWKS (refetched hourly, and on an unknown `kid` at most every 30 s) and checks the
  signature, `iss`, `aud` (must contain `ROSTWERK_OIDC_AUDIENCE`), `exp` and `nbf`
  (30 s leeway) and `sub`. The algorithm comes from the key, never from the token;
  `none` and HMAC tokens are rejected. Any standards-compliant provider works; the
  dev setup uses Keycloak.
- **Dev tokens** `dev:<tenant-id>:<user>`, only with `ROSTWERK_DEV_AUTH=1`.

### From token to tenant

The identity provider says *who* is calling; Rostwerk decides *where* they may act.
The token carries no tenant. Instead the core table `tenant_members` maps the token's
`sub` to tenants, with a kind per membership:

| kind | Actor in audit and outbox |
| --- | --- |
| `user` | `user:<sub>` |
| `agent` | `agent:<sub>` |

Agents (machine identities, usually client credentials tokens) are admitted as
`agent`, so their changes are never attributed to a human. The actor is the `sub`,
because it is stable at the issuer; display names stay in the identity provider.

Why a table and not a tenant claim: memberships are business data that the operator
(and later the tenant admin) manages through the API, the same identity can belong to
several tenants (consultants, tax advisors, agents serving many tenants), and no
provider-specific token mappers are needed.

Selecting the tenant per request:

| Situation | Result |
| --- | --- |
| Missing, malformed, expired or foreign token | `401` |
| Valid token, member of exactly one tenant | that tenant |
| Valid token, several tenants, no header | `400`, set `X-Rostwerk-Tenant` |
| `X-Rostwerk-Tenant: <tenant-id>` and a member there | that tenant |
| No membership, or not a member of the requested tenant | `403` (unknown and foreign tenants look the same) |

Manage memberships through the admin API:

```bash
# Admit a subject (the token's `sub`) as user or agent
curl -X PUT localhost:8080/admin/tenants/<tenant-id>/members/<sub> \
  -H 'Authorization: Bearer dev-admin' -H 'content-type: application/json' \
  -d '{"kind":"user"}'
curl localhost:8080/admin/tenants/<tenant-id>/members -H 'Authorization: Bearer dev-admin'
curl -X DELETE localhost:8080/admin/tenants/<tenant-id>/members/<sub> -H 'Authorization: Bearer dev-admin'
```

## Audit log and events

Every change to an audited table is journaled by a database trigger in the same
transaction (append-only, read it via `GET /v1/audit`). Modules record domain events
with `tx.publish(topic, &payload)`; they are stored in an outbox only if the
transaction commits and then relayed at least once. Details:
[docs/architecture.md](docs/architecture.md#audit-log).

With `NATS_URL` set, events go to NATS JetStream (stream `ROSTWERK`, created on startup
if missing), otherwise to the log. Each event is published on
`rostwerk.<tenant_id>.<topic>` as JSON (`id`, `tenant_id`, `topic`, `payload`, `actor`,
`occurred_at`) with the event id as `Nats-Msg-Id`, so JetStream drops redeliveries
within its duplicate window (2 minutes by default); consumers should still dedupe on `id`.

## Dry runs

Every write endpoint under `/v1` can be previewed: send `Prefer: dry-run` and the call
runs exactly as usual (validation, database constraints, audit trigger, events) and is
then rolled back, so nothing is stored. The response has the status the real call
would return, the header `Preference-Applied: dry-run`, and the body

```json
{ "result": { "...": "the normal response body" },
  "events": [{ "topic": "masterdata.partner.created", "payload": { "...": "..." } }] }
```

Errors come back exactly as for a real call. Endpoints without dry-run support (admin)
ignore the preference and send no `Preference-Applied`, so check for that header.

```bash
curl -i -X POST localhost:8080/v1/partners -H 'Prefer: dry-run' \
  -H 'Authorization: Bearer dev:<tenant-id>:alice' -H 'content-type: application/json' \
  -d '{"name":"ACME","kind":"customer"}'
```

In a module, a write handler takes the `core_host::DryRun` extractor, adds `params(DryRun)`
to its `#[utoipa::path]`, and ends with `dry_run.respond(tx, status, body)` instead of
`tx.commit()`. A test fails if a `/v1` write endpoint does not declare it.
Commands of other modules are called with `bus.command_in(&ctx, &mut tx, ...)`, so they
run in the handler's transaction and are rolled back with it, in a dry run as well.

## Custom fields

Modules declare which entities are extensible (`GET /v1/ext-entities`, e.g.
`masterdata.partner`). Each tenant defines fields on them, identified by entity and key,
never by a generated id, so a field is the same on every system:

```bash
curl -X PUT localhost:8080/v1/ext-fields/masterdata.partner/credit_limit \
  -H 'Authorization: Bearer dev:<tenant-id>:alice' -H 'content-type: application/json' \
  -d '{"type":"decimal","label":"Credit limit","required":false}'
```

Types: `string`, `number`, `decimal` (string, e.g. `"1500.00"`), `boolean`, `date`
(`YYYY-MM-DD`) and `enum` (with `enum_values`). Keys match `^[a-z][a-z0-9_]{0,62}$`;
`schema` is reserved. `PUT` creates or replaces and is idempotent: the same definition
again changes nothing. Values go into the entity's `ext` object and are validated on
every write (`422` naming the field, e.g. `ext.credit_limit: must be a decimal string`).
`GET /v1/ext-fields/{entity}/schema` returns the effective JSON Schema.

Changing a definition does not migrate stored values. Removing a field keeps its values
in the database, but they are no longer returned; defining it again brings them back.

Customizing can be changed directly only on a Dev system (`ROSTWERK_SYSTEM_ROLE=dev`).
QAS and PROD answer `PUT`/`DELETE /v1/ext-fields/...` with `409`: they receive
customizing only through transports. Reads and business data are unaffected.

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

The OpenAPI spec is served at `GET /openapi.json`, interactive docs at `/docs`.

### Signing in with OIDC (Keycloak)

`docker-compose up -d` also starts Keycloak on <http://localhost:8180> (admin console:
`admin` / `admin`). On first start it imports
[`docker/keycloak/rostwerk-realm.json`](docker/keycloak/rostwerk-realm.json), so there
is nothing to click:

| What | Value |
| --- | --- |
| Realm / issuer | `rostwerk` / `http://localhost:8180/realms/rostwerk` |
| User | `alice` / `alice`, `sub` = `a11ce000-0000-4000-8000-000000000001` |
| Client for `/docs` and curl | `rostwerk-docs` (public, authorization code + PKCE; password grant in this dev realm only) |
| Client for agents | `rostwerk-agent` / secret `rostwerk-agent-secret`, `sub` = `a6e47000-0000-4000-8000-000000000001` |
| Audience | `rostwerk-api` (added to tokens of both clients) |

Keycloak was chosen over Zitadel because a single container with `--import-realm`
gives a fully reproducible realm with fixed user ids, client secrets and an audience
mapper, without a separate database or API calls to bootstrap. Keycloak dev mode keeps
its state in the container; `docker-compose up -d --force-recreate keycloak` resets it.

Admit alice and the agent to the tenant created above:

```bash
curl -X PUT localhost:8080/admin/tenants/<tenant-id>/members/a11ce000-0000-4000-8000-000000000001 \
  -H 'Authorization: Bearer dev-admin' -H 'content-type: application/json' -d '{"kind":"user"}'
curl -X PUT localhost:8080/admin/tenants/<tenant-id>/members/a6e47000-0000-4000-8000-000000000001 \
  -H 'Authorization: Bearer dev-admin' -H 'content-type: application/json' -d '{"kind":"agent"}'
```

Get a token and call the API:

```bash
TOKEN=$(curl -s http://localhost:8180/realms/rostwerk/protocol/openid-connect/token \
  -d grant_type=password -d client_id=rostwerk-docs -d username=alice -d password=alice \
  | jq -r .access_token)
curl -X POST localhost:8080/v1/partners -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"name":"ACME","kind":"customer"}'

# As the agent (client credentials)
AGENT_TOKEN=$(curl -s http://localhost:8180/realms/rostwerk/protocol/openid-connect/token \
  -d grant_type=client_credentials -d client_id=rostwerk-agent -d client_secret=rostwerk-agent-secret \
  | jq -r .access_token)
```

In `/docs`, click **Authorize**, then under **oidc** tick `openid` and sign in as
alice (`client_id` is prefilled). Alternatively paste a token into **bearer**.
The server looks up the issuer's sign-in endpoints at startup; if Keycloak is not
running then, `/docs` offers only the **bearer** field (restart the server once
Keycloak is up). Token checks do not depend on this and work as soon as the
issuer is reachable. If your identity belongs to
several tenants, also fill in **tenant** (`X-Rostwerk-Tenant`). The redirect URIs in
the dev realm allow `http://localhost:8080/docs/*` only, so open the docs on that
address.

## Tests

```bash
cargo test
```

The integration tests in `apps/api-server/tests` need the Postgres from
`docker-compose` and the URLs from `.env`. Each test creates its own database
(`rostwerk_test_<id>`), migrates it and drops it afterwards, so tests are
independent of each other and of a running dev server.

OIDC tests need no identity provider: unit tests sign tokens with keys generated in
the test, and the integration tests start a fake issuer on a local port that serves
discovery and a JWKS. Keycloak is not needed in CI.

Without database URLs the tests are skipped. In CI set `ROSTWERK_REQUIRE_DB=1`
so they fail instead. The NATS tests also need `NATS_URL` and are skipped without it,
unless `ROSTWERK_REQUIRE_NATS=1`.

## Configuration

| Variable | Meaning |
| --- | --- |
| `DATABASE_URL` | App connection, role subject to RLS |
| `DATABASE_ADMIN_URL` | Connection used for migrations |
| `BIND` | Listen address, default `127.0.0.1:8080` |
| `NATS_URL` | NATS server for outbox events, e.g. `nats://localhost:4222`. Unset logs events instead |
| `ROSTWERK_DEV_AUTH` | `1` accepts `dev:<tenant>:<user>` bearer tokens. Development only |
| `ROSTWERK_ADMIN_TOKEN` | Bearer token for `/admin`. Unset disables the admin API |
| `ROSTWERK_OIDC_ISSUER` | OIDC issuer URL, exactly as in the tokens' `iss`. Unset disables OIDC |
| `ROSTWERK_OIDC_AUDIENCE` | Required `aud` value of access tokens, e.g. `rostwerk-api` |
| `ROSTWERK_OIDC_DOCS_CLIENT_ID` | Public client `/docs` signs in with, e.g. `rostwerk-docs` |
| `ROSTWERK_SYSTEM_ROLE` | `dev` (default), `qas` or `prod`. Only `dev` allows changing customizing (custom fields) directly; `qas` and `prod` answer `409` |

## Contributing

Pick an issue labeled `ready` and follow [AGENTS.md](AGENTS.md) (pickup protocol) and
[CONTRIBUTING.md](CONTRIBUTING.md) (code rules).

## License

Rostwerk's code is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option. You may run, host, modify and sell services around it, including commercially.

The name "Rostwerk", its logos and the look of its official user interfaces are covered
separately by the [brand licence](LICENSE-BRAND): build on the code freely, but don't
present a modified product as Rostwerk without asking.

Contributions are accepted under the same terms; see [CONTRIBUTING.md](CONTRIBUTING.md).
