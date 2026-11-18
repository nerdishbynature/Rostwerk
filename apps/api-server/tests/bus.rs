//! The in-process query and command bus between modules.

mod common;

use axum::http::StatusCode;
use common::{call, create_tenant, harness, token};
use core_host::{
    CoreState, Module, ModuleManifest, ModuleRegistry, Operation, OperationKind, Operations,
};
use core_kernel::{Actor, Error, Result, TenantCtx, TenantId, TenantTx};
use rostwerk_api::registry;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use uuid::Uuid;

/// What a calling module (e.g. sales) brings as its own view of a partner.
/// It does not depend on the masterdata crate.
#[derive(Debug, Deserialize)]
struct PartnerView {
    id: Uuid,
    name: String,
}

fn ctx(tenant: &str, modules: &[&str]) -> TenantCtx {
    TenantCtx::new(
        TenantId(tenant.parse().unwrap()),
        Actor::Agent("bus-test".into()),
        modules.iter().map(|m| m.to_string()),
    )
}

async fn create_partner(h: &common::Harness, tenant: &str, name: &str) -> Uuid {
    let (status, partner) = call(
        &h.app,
        "POST",
        "/v1/partners",
        &token(tenant),
        Some(json!({ "name": name, "kind": "customer" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{partner}");
    partner["id"].as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn partner_get_query() {
    let Some(h) = harness().await else { return };
    let bus = registry().bus(h.db.clone());
    let a = create_tenant(&h.app, "Tenant A", &["masterdata"]).await;
    let id = create_partner(&h, &a, "ACME GmbH").await;

    let partner: PartnerView = bus
        .query(
            &ctx(&a, &["masterdata"]),
            "masterdata.partner.get",
            &json!({ "id": id }),
        )
        .await
        .unwrap();
    assert_eq!(partner.id, id);
    assert_eq!(partner.name, "ACME GmbH");

    // Same operation as JSON, as an agent tool call would send it.
    let partner = bus
        .call_json(
            &ctx(&a, &["masterdata"]),
            "masterdata.partner.get",
            json!({ "id": id }),
        )
        .await
        .unwrap();
    assert_eq!(partner["kind"], "customer");

    // Unknown id and malformed request.
    let missing = bus
        .query::<_, PartnerView>(
            &ctx(&a, &["masterdata"]),
            "masterdata.partner.get",
            &json!({ "id": Uuid::now_v7() }),
        )
        .await;
    assert!(matches!(missing, Err(Error::NotFound)), "{missing:?}");
    let malformed = bus
        .query::<_, PartnerView>(
            &ctx(&a, &["masterdata"]),
            "masterdata.partner.get",
            &json!({ "id": "not-a-uuid" }),
        )
        .await;
    assert!(
        matches!(malformed, Err(Error::Validation(_))),
        "{malformed:?}"
    );
}

#[tokio::test]
async fn query_runs_in_callers_tenant() {
    let Some(h) = harness().await else { return };
    let bus = registry().bus(h.db.clone());
    let a = create_tenant(&h.app, "Tenant A", &["masterdata"]).await;
    let b = create_tenant(&h.app, "Tenant B", &["masterdata"]).await;
    let id = create_partner(&h, &a, "ACME GmbH").await;

    // B asks for A's partner by id: RLS hides it.
    let result = bus
        .query::<_, PartnerView>(
            &ctx(&b, &["masterdata"]),
            "masterdata.partner.get",
            &json!({ "id": id }),
        )
        .await;
    assert!(matches!(result, Err(Error::NotFound)), "{result:?}");
}

#[tokio::test]
async fn disabled_module_has_no_operations() {
    let Some(h) = harness().await else { return };
    let bus = registry().bus(h.db.clone());
    let a = create_tenant(&h.app, "Tenant A", &["masterdata"]).await;
    let id = create_partner(&h, &a, "ACME GmbH").await;
    let off = create_tenant(&h.app, "Tenant without modules", &[]).await;

    for ctx in [ctx(&a, &[]), ctx(&off, &[])] {
        let result = bus
            .query::<_, PartnerView>(&ctx, "masterdata.partner.get", &json!({ "id": id }))
            .await;
        assert!(matches!(result, Err(Error::NotFound)), "{result:?}");
    }

    // Unknown names and the wrong kind are not found either.
    let ctx = ctx(&a, &["masterdata"]);
    let unknown = bus
        .query::<_, PartnerView>(&ctx, "masterdata.partner.nope", &json!({ "id": id }))
        .await;
    assert!(matches!(unknown, Err(Error::NotFound)), "{unknown:?}");
    let wrong_kind = bus
        .command::<_, PartnerView>(&ctx, "masterdata.partner.get", &json!({ "id": id }))
        .await;
    assert!(matches!(wrong_kind, Err(Error::NotFound)), "{wrong_kind:?}");
}

#[tokio::test]
async fn lists_operations_with_schemas() {
    let ops = registry().operations();
    let get = ops
        .iter()
        .find(|op| op.name == "masterdata.partner.get")
        .expect("masterdata.partner.get registered");
    assert_eq!(get.module, "masterdata");
    assert_eq!(get.kind, OperationKind::Query);
    assert!(!get.description.is_empty());

    let spec = serde_json::to_value(get).unwrap();
    assert_eq!(spec["kind"], "query");
    assert_eq!(spec["request"]["schema"]["required"], json!(["id"]));
    assert_eq!(
        spec["request"]["schema"]["properties"]["id"]["format"],
        "uuid"
    );
    let response = &spec["response"]["schema"]["properties"];
    assert!(response["name"].is_object(), "{spec}");
    // Referenced types travel with the schema.
    assert_eq!(
        response["kind"]["$ref"], "#/components/schemas/PartnerKind",
        "{spec}"
    );
    assert!(spec["response"]["components"]["PartnerKind"].is_object());
}

// A module with a command, to show commands run in one transaction with their events.

const TEST_MODULE: &str = "bustest";
const EMITTED: &str = "bustest.signal.emitted";

struct BusTest;

impl Module for BusTest {
    fn manifest(&self) -> ModuleManifest {
        ModuleManifest {
            id: TEST_MODULE,
            name: "Bus test",
            version: "0",
            description: "Test operations",
        }
    }

    fn routes(&self) -> OpenApiRouter<CoreState> {
        OpenApiRouter::new()
    }

    fn operations(&self, ops: &mut Operations) {
        ops.add(Emit).add(EmitFromQuery);
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
struct EmitRequest {
    /// Fail after publishing.
    fail: bool,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct Emitted {
    event_id: Uuid,
}

struct Emit;

impl Operation for Emit {
    const NAME: &'static str = "bustest.signal.emit";
    const KIND: OperationKind = OperationKind::Command;
    const DESCRIPTION: &'static str = "Publish an event, then optionally fail.";
    type Request = EmitRequest;
    type Response = Emitted;

    async fn run(&self, tx: &mut TenantTx, req: EmitRequest) -> Result<Emitted> {
        let event_id = tx.publish(EMITTED, &json!({})).await?;
        if req.fail {
            return Err(Error::Validation("asked to fail".into()));
        }
        Ok(Emitted { event_id })
    }
}

/// A query that tries to write. The bus runs queries read-only.
struct EmitFromQuery;

impl Operation for EmitFromQuery {
    const NAME: &'static str = "bustest.signal.sneak";
    const KIND: OperationKind = OperationKind::Query;
    const DESCRIPTION: &'static str = "Tries to publish from a query.";
    type Request = EmitRequest;
    type Response = Emitted;

    async fn run(&self, tx: &mut TenantTx, _req: EmitRequest) -> Result<Emitted> {
        let event_id = tx.publish(EMITTED, &json!({})).await?;
        Ok(Emitted { event_id })
    }
}

async fn emitted_count(h: &common::Harness, tenant: &str) -> i64 {
    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM outbox WHERE tenant_id = $1::uuid AND topic = $2")
            .bind(tenant)
            .bind(EMITTED)
            .fetch_one(&h.admin_pool)
            .await
            .unwrap();
    count
}

#[tokio::test]
async fn commands_publish_atomically() {
    let Some(h) = harness().await else { return };
    let bus = ModuleRegistry::new().register(BusTest).bus(h.db.clone());
    let t = create_tenant(&h.app, "Tenant commands", &[]).await;
    let ctx = ctx(&t, &[TEST_MODULE]);

    // Success: the event is in the outbox.
    let emitted: Emitted = bus
        .command(&ctx, "bustest.signal.emit", &json!({ "fail": false }))
        .await
        .unwrap();
    assert_eq!(emitted_count(&h, &t).await, 1);
    let (actor,): (String,) = sqlx::query_as("SELECT actor FROM outbox WHERE id = $1")
        .bind(emitted.event_id)
        .fetch_one(&h.admin_pool)
        .await
        .unwrap();
    assert_eq!(actor, "agent:bus-test");

    // Failure after publishing: rolled back, no event.
    let failed = bus
        .command::<_, Emitted>(&ctx, "bustest.signal.emit", &json!({ "fail": true }))
        .await;
    assert!(matches!(failed, Err(Error::Validation(_))));
    assert_eq!(emitted_count(&h, &t).await, 1);

    // A query cannot write.
    let sneaked = bus
        .query::<_, Emitted>(&ctx, "bustest.signal.sneak", &json!({ "fail": false }))
        .await;
    assert!(matches!(sneaked, Err(Error::Db(_))), "{:?}", sneaked.err());
    assert_eq!(emitted_count(&h, &t).await, 1);
}

#[test]
#[should_panic(expected = "must be named bustest.")]
fn operations_stay_in_their_module_namespace() {
    struct Squatter;
    impl Module for Squatter {
        fn manifest(&self) -> ModuleManifest {
            ModuleManifest {
                id: TEST_MODULE,
                name: "Squatter",
                version: "0",
                description: "Registers a masterdata name",
            }
        }
        fn routes(&self) -> OpenApiRouter<CoreState> {
            OpenApiRouter::new()
        }
        fn operations(&self, ops: &mut Operations) {
            struct Fake;
            impl Operation for Fake {
                const NAME: &'static str = "masterdata.partner.delete";
                const KIND: OperationKind = OperationKind::Command;
                const DESCRIPTION: &'static str = "";
                type Request = EmitRequest;
                type Response = Emitted;
                async fn run(&self, _tx: &mut TenantTx, _req: EmitRequest) -> Result<Emitted> {
                    Err(Error::Forbidden)
                }
            }
            ops.add(Fake);
        }
    }
    ModuleRegistry::new().register(Squatter).operations();
}
