//! Bus commands called from a write handler run in the handler's transaction
//! (`Bus::command_in`): they commit, roll back and dry-run together with it.
//!
//! Two test modules: `bustxorders` has a write endpoint that places an order and
//! reserves the item through a command of `bustxstock`.

mod common;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use common::{ADMIN, Harness, call, call_with, create_tenant, harness_for, token};
use core_host::{CoreState, DryRun, Module, ModuleManifest, ModuleRegistry, Operation};
use core_host::{OperationKind, Operations};
use core_kernel::{Actor, Error, Result, TenantCtx, TenantId, TenantTx, new_id};
use rostwerk_api::AuthConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::migrate::Migrator;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use uuid::Uuid;

const ORDERS: &str = "bustxorders";
const STOCK: &str = "bustxstock";
const PLACED: &str = "bustxorders.order.placed";
const RESERVED: &str = "bustxstock.item.reserved";
const RESERVE: &str = "bustxstock.item.reserve";

// --- Module `bustxstock`: offers a command, owns `bustx_reservations`.

struct Stock;

impl Module for Stock {
    fn manifest(&self) -> ModuleManifest {
        ModuleManifest {
            id: STOCK,
            name: "Bus tx stock",
            version: "0",
            description: "Test module with a command",
        }
    }

    fn migrator(&self) -> Option<Migrator> {
        Some(sqlx::migrate!("./tests/fixtures/bustx_stock"))
    }

    fn routes(&self) -> OpenApiRouter<CoreState> {
        // The registry cannot gate a module without routes yet, so offer one.
        OpenApiRouter::new().route("/v1/bustx/stock", get(|| async { "ok" }))
    }

    fn operations(&self, ops: &mut Operations) {
        ops.add(Reserve);
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
struct ReserveRequest {
    item: String,
    /// Fail after writing and publishing.
    fail: bool,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct Reservation {
    id: Uuid,
}

struct Reserve;

impl Operation for Reserve {
    const NAME: &'static str = RESERVE;
    const KIND: OperationKind = OperationKind::Command;
    const DESCRIPTION: &'static str = "Reserve an item, then optionally fail.";
    type Request = ReserveRequest;
    type Response = Reservation;

    async fn run(&self, tx: &mut TenantTx, req: ReserveRequest) -> Result<Reservation> {
        let id = new_id();
        sqlx::query("INSERT INTO bustx_reservations (id, tenant_id, item) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(tx.tenant_id().0)
            .bind(&req.item)
            .execute(tx.conn())
            .await?;
        tx.publish(RESERVED, &json!({ "id": id, "item": req.item }))
            .await?;
        if req.fail {
            return Err(Error::Conflict("out of stock".into()));
        }
        Ok(Reservation { id })
    }
}

// --- Module `bustxorders`: a write endpoint that calls the stock command.

struct Orders;

impl Module for Orders {
    fn manifest(&self) -> ModuleManifest {
        ModuleManifest {
            id: ORDERS,
            name: "Bus tx orders",
            version: "0",
            description: "Test module whose write endpoint calls a command",
        }
    }

    fn migrator(&self) -> Option<Migrator> {
        Some(sqlx::migrate!("./tests/fixtures/bustx_orders"))
    }

    fn routes(&self) -> OpenApiRouter<CoreState> {
        OpenApiRouter::new().route("/v1/bustx/orders", post(place_order))
    }
}

#[derive(Deserialize)]
struct NewOrder {
    item: String,
    /// The stock command fails; the order is placed without a reservation.
    #[serde(default)]
    reserve_fails: bool,
    /// The handler fails after the command succeeded.
    #[serde(default)]
    fail_after: bool,
}

async fn place_order(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    dry_run: DryRun,
    Json(new): Json<NewOrder>,
) -> Result<Response> {
    let mut tx = ctx.begin(&state.db).await?;
    let id = new_id();
    sqlx::query("INSERT INTO bustx_orders (id, tenant_id, item) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(tx.tenant_id().0)
        .bind(&new.item)
        .execute(tx.conn())
        .await?;
    tx.publish(PLACED, &json!({ "id": id })).await?;

    let reservation = state
        .bus
        .command_in::<_, Reservation>(
            &ctx,
            &mut tx,
            RESERVE,
            &json!({ "item": new.item, "fail": new.reserve_fails }),
        )
        .await;
    let reservation = match reservation {
        Ok(r) => Some(r.id),
        // The caller may handle a failed command and go on.
        Err(Error::Conflict(_)) => None,
        Err(e) => return Err(e),
    };
    if new.fail_after {
        return Err(Error::Validation("failed after reserving".into()));
    }
    dry_run
        .respond(
            tx,
            StatusCode::CREATED,
            json!({ "id": id, "reservation": reservation }),
        )
        .await
}

// --- Tests

fn test_registry() -> ModuleRegistry {
    ModuleRegistry::new().register(Orders).register(Stock)
}

async fn setup() -> Option<(Harness, String)> {
    let h = harness_for(
        test_registry,
        AuthConfig {
            dev_tokens: true,
            admin_token: Some(ADMIN.into()),
            ..Default::default()
        },
    )
    .await?;
    let t = create_tenant(&h.app, "Bus tx", &[ORDERS, STOCK]).await;
    Some((h, t))
}

async fn place(
    h: &Harness,
    tenant: &str,
    prefer: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let headers: Vec<(&str, &str)> = prefer.map(|p| ("prefer", p)).into_iter().collect();
    call_with(
        &h.app,
        "POST",
        "/v1/bustx/orders",
        &token(tenant),
        &headers,
        Some(body),
    )
    .await
}

/// Rows of the tenant in a table, counted as the owner role (bypasses RLS).
async fn count(h: &Harness, table: &str, tenant: &str) -> i64 {
    let (n,): (i64,) = sqlx::query_as(&format!(
        "SELECT count(*) FROM {table} WHERE tenant_id = $1::uuid"
    ))
    .bind(tenant)
    .fetch_one(&h.admin_pool)
    .await
    .unwrap();
    n
}

/// Rows in `bustx_orders`, `bustx_reservations`, `audit_log` and `outbox`.
async fn counts(h: &Harness, tenant: &str) -> [i64; 4] {
    [
        count(h, "bustx_orders", tenant).await,
        count(h, "bustx_reservations", tenant).await,
        count(h, "audit_log", tenant).await,
        count(h, "outbox", tenant).await,
    ]
}

async fn outbox_topics(h: &Harness, tenant: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT topic FROM outbox WHERE tenant_id = $1::uuid ORDER BY id")
        .bind(tenant)
        .fetch_all(&h.admin_pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn command_commits_with_the_caller() {
    let Some((h, t)) = setup().await else { return };

    let (status, body) = place(&h, &t, None, json!({ "item": "bolt" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["reservation"].is_string(), "{body}");

    // Order and reservation, an audit entry each, both events.
    assert_eq!(counts(&h, &t).await, [1, 1, 2, 2]);
    assert_eq!(outbox_topics(&h, &t).await, [PLACED, RESERVED]);

    // Both in the same database transaction.
    let (txids,): (i64,) =
        sqlx::query_as("SELECT count(DISTINCT txid) FROM audit_log WHERE tenant_id = $1::uuid")
            .bind(&t)
            .fetch_one(&h.admin_pool)
            .await
            .unwrap();
    assert_eq!(txids, 1);
}

#[tokio::test]
async fn dry_run_rolls_back_the_command() {
    let Some((h, t)) = setup().await else { return };

    let (status, body) = place(&h, &t, Some("dry-run"), json!({ "item": "bolt" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["result"]["reservation"].is_string(), "{body}");

    // The command's event is in the response, after the caller's.
    let topics: Vec<&str> = body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["topic"].as_str().unwrap())
        .collect();
    assert_eq!(topics, [PLACED, RESERVED], "{body}");
    assert_eq!(
        body["events"][1]["payload"]["id"], body["result"]["reservation"],
        "{body}"
    );

    // No rows, audit entries or outbox events of either module.
    assert_eq!(counts(&h, &t).await, [0, 0, 0, 0]);
}

#[tokio::test]
async fn caller_failure_rolls_back_the_command() {
    let Some((h, t)) = setup().await else { return };

    let (status, body) = place(&h, &t, None, json!({ "item": "bolt", "fail_after": true })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(counts(&h, &t).await, [0, 0, 0, 0]);
}

#[tokio::test]
async fn failed_command_undoes_only_its_own_work() {
    let Some((h, t)) = setup().await else { return };

    for prefer in [Some("dry-run"), None] {
        let (status, body) = place(
            &h,
            &t,
            prefer,
            json!({ "item": "bolt", "reserve_fails": true }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        if prefer.is_some() {
            // The failed command's event is gone from the dry-run response too.
            assert_eq!(body["result"]["reservation"], Value::Null, "{body}");
            assert_eq!(body["events"].as_array().unwrap().len(), 1, "{body}");
            assert_eq!(body["events"][0]["topic"], PLACED, "{body}");
            assert_eq!(counts(&h, &t).await, [0, 0, 0, 0]);
        } else {
            assert_eq!(body["reservation"], Value::Null, "{body}");
        }
    }

    // The order is placed; the reservation, its audit entry and its event are not.
    assert_eq!(counts(&h, &t).await, [1, 0, 1, 1]);
    assert_eq!(outbox_topics(&h, &t).await, [PLACED]);
}

#[tokio::test]
async fn command_in_checks_tenant_and_module() {
    let Some((h, t)) = setup().await else { return };
    let other = create_tenant(&h.app, "Other", &[ORDERS, STOCK]).await;
    let ctx = |tenant: &str, modules: &[&str]| {
        TenantCtx::new(
            TenantId(tenant.parse().unwrap()),
            Actor::Agent("bus-tx-test".into()),
            modules.iter().map(|m| m.to_string()),
        )
    };
    let bus = test_registry().bus(h.db.clone());
    let req = json!({ "item": "bolt", "fail": false });

    // A transaction of another tenant is refused.
    let mut foreign = ctx(&other, &[]).begin(&h.db).await.unwrap();
    let result = bus
        .command_in::<_, Reservation>(&ctx(&t, &[STOCK]), &mut foreign, RESERVE, &req)
        .await;
    assert!(
        matches!(result, Err(Error::Forbidden)),
        "{:?}",
        result.err()
    );

    // A module the tenant has not enabled does not exist.
    let mut tx = ctx(&t, &[]).begin(&h.db).await.unwrap();
    let result = bus
        .command_in::<_, Reservation>(&ctx(&t, &[ORDERS]), &mut tx, RESERVE, &req)
        .await;
    assert!(matches!(result, Err(Error::NotFound)), "{:?}", result.err());
    drop((foreign, tx));

    // The tenant's module routes stay behind the gate as usual.
    let (status, _) = call(
        &h.app,
        "POST",
        "/v1/bustx/orders",
        &token(&create_tenant(&h.app, "Off", &[]).await),
        Some(json!({ "item": "bolt" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(counts(&h, &t).await, [0, 0, 0, 0]);
}
