//! Audit log (trigger-written, append-only, tenant-scoped) and transactional outbox.

mod common;

use std::sync::Mutex;

use axum::http::StatusCode;
use common::{call, create_tenant, harness, token};
use core_host::outbox::{Event, EventSink, SinkFuture, relay_once};
use core_kernel::{Actor, TenantCtx, TenantId};
use serde_json::{Value, json};
use uuid::Uuid;

async fn create_partner(app: &axum::Router, tenant: &str, name: &str) -> Value {
    let (status, partner) = call(
        app,
        "POST",
        "/v1/partners",
        &token(tenant),
        Some(json!({ "name": name, "kind": "customer" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{partner}");
    partner
}

#[tokio::test]
async fn audit_records_create_and_update_with_actor() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Audit A", &["masterdata"]).await;
    let partner = create_partner(&h.app, &t, "Alt GmbH").await;
    let id = partner["id"].as_str().unwrap();

    let (status, _) = call(
        &h.app,
        "PATCH",
        &format!("/v1/partners/{id}"),
        &token(&t),
        Some(json!({ "name": "Neu GmbH" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, page) = call(
        &h.app,
        "GET",
        &format!("/v1/audit?entity_id={id}"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{page}");

    assert_eq!(items[0]["action"], "insert");
    assert_eq!(items[0]["entity_type"], "partners");
    assert_eq!(items[0]["actor"], "user:tester");
    assert_eq!(items[0]["after"]["name"], "Alt GmbH");
    assert_eq!(items[0]["before"], Value::Null);

    assert_eq!(items[1]["action"], "update");
    assert_eq!(items[1]["before"]["name"], "Alt GmbH");
    assert_eq!(items[1]["after"]["name"], "Neu GmbH");
}

#[tokio::test]
async fn audit_is_tenant_scoped() {
    let Some(h) = harness().await else { return };
    let a = create_tenant(&h.app, "Audit scope A", &["masterdata"]).await;
    let b = create_tenant(&h.app, "Audit scope B", &["masterdata"]).await;
    let partner = create_partner(&h.app, &a, "Geheim KG").await;
    let id = partner["id"].as_str().unwrap();

    let (status, page) = call(
        &h.app,
        "GET",
        &format!("/v1/audit?entity_id={id}"),
        &token(&b),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"], json!([]));
}

#[tokio::test]
async fn audit_log_is_append_only() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Audit immutable", &["masterdata"]).await;
    create_partner(&h.app, &t, "Fest AG").await;

    // Not even the owner role may rewrite history.
    let update = sqlx::query("UPDATE audit_log SET actor = 'forged' WHERE tenant_id = $1::uuid")
        .bind(&t)
        .execute(&h.admin_pool)
        .await;
    assert!(update.is_err(), "update must fail");
    let delete = sqlx::query("DELETE FROM audit_log WHERE tenant_id = $1::uuid")
        .bind(&t)
        .execute(&h.admin_pool)
        .await;
    assert!(delete.is_err(), "delete must fail");

    // The app cannot forge entries either, even inside its own tenant.
    let ctx = TenantCtx::new(
        TenantId(Uuid::parse_str(&t).unwrap()),
        Actor::User("tester".into()),
        Vec::<String>::new(),
    );
    let mut tx = ctx.begin(&h.db).await.unwrap();
    let forged = sqlx::query(
        "INSERT INTO audit_log (tenant_id, actor, entity_type, entity_id, action)
         VALUES ($1::uuid, 'forged', 'partners', gen_random_uuid(), 'insert')",
    )
    .bind(&t)
    .execute(tx.conn())
    .await;
    assert!(forged.is_err(), "direct insert must fail");
}

#[tokio::test]
async fn outbox_event_is_written_with_the_change() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Outbox A", &["masterdata"]).await;
    let partner = create_partner(&h.app, &t, "Event GmbH").await;

    let (topic, payload, actor): (String, Value, String) = sqlx::query_as(
        "SELECT topic, payload, actor FROM outbox WHERE tenant_id = $1::uuid ORDER BY id",
    )
    .bind(&t)
    .fetch_one(&h.admin_pool)
    .await
    .unwrap();
    assert_eq!(topic, "masterdata.partner.created");
    assert_eq!(payload["id"], partner["id"]);
    assert_eq!(actor, "user:tester");
}

#[tokio::test]
async fn rolled_back_transaction_leaves_no_event() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Outbox rollback", &[]).await;
    let ctx = TenantCtx::new(
        TenantId(Uuid::parse_str(&t).unwrap()),
        Actor::User("tester".into()),
        Vec::<String>::new(),
    );

    let mut tx = ctx.begin(&h.db).await.unwrap();
    let event_id = tx
        .publish("test.thing.happened", &json!({ "x": 1 }))
        .await
        .unwrap();
    drop(tx); // no commit -> rollback

    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM outbox WHERE id = $1")
        .bind(event_id)
        .fetch_one(&h.admin_pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn app_role_cannot_read_other_tenants_events() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Outbox RLS", &["masterdata"]).await;
    create_partner(&h.app, &t, "Privat GmbH").await;

    // Outside a tenant or system transaction the outbox is invisible.
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM outbox")
        .fetch_one(&h.app_pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[derive(Default)]
struct Collect(Mutex<Vec<Event>>);

impl EventSink for Collect {
    fn publish<'a>(&'a self, event: &'a Event) -> SinkFuture<'a> {
        self.0.lock().unwrap().push(event.clone());
        Box::pin(async { Ok(()) })
    }
}

/// Accepts the first `ok` events, then fails.
struct FailAfter {
    ok: usize,
    seen: Mutex<usize>,
}

impl EventSink for FailAfter {
    fn publish<'a>(&'a self, _event: &'a Event) -> SinkFuture<'a> {
        let mut seen = self.seen.lock().unwrap();
        *seen += 1;
        let fail = *seen > self.ok;
        Box::pin(async move {
            if fail {
                anyhow::bail!("sink down")
            }
            Ok(())
        })
    }
}

async fn pending_events(h: &common::Harness) -> i64 {
    let (pending,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM outbox WHERE published_at IS NULL")
            .fetch_one(&h.admin_pool)
            .await
            .unwrap();
    pending
}

/// Creates a partner and renames it: two events, created then updated.
async fn two_events(h: &common::Harness) -> Value {
    let t = create_tenant(&h.app, "Relay", &["masterdata"]).await;
    let partner = create_partner(&h.app, &t, "Relay GmbH").await;
    let id = partner["id"].as_str().unwrap();
    let (status, _) = call(
        &h.app,
        "PATCH",
        &format!("/v1/partners/{id}"),
        &token(&t),
        Some(json!({ "name": "Relay AG" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    partner
}

#[tokio::test]
async fn relay_publishes_in_order_and_only_once() {
    let Some(h) = harness().await else { return };
    let partner = two_events(&h).await;

    let sink = Collect::default();
    assert_eq!(relay_once(&h.db, &sink, 100).await.unwrap(), 2);
    assert_eq!(pending_events(&h).await, 0);

    let seen = sink.0.lock().unwrap().clone();
    let topics: Vec<_> = seen.iter().map(|e| e.topic.as_str()).collect();
    assert_eq!(
        topics,
        ["masterdata.partner.created", "masterdata.partner.updated"]
    );
    assert_eq!(seen[0].payload["id"], partner["id"]);
    assert_eq!(seen[1].payload["name"], "Relay AG");
    assert_eq!(seen[0].actor, "user:tester");

    // Nothing left: a second run publishes nothing.
    assert_eq!(relay_once(&h.db, &sink, 100).await.unwrap(), 0);
    assert_eq!(sink.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn relay_keeps_events_the_sink_rejected() {
    let Some(h) = harness().await else { return };
    two_events(&h).await;

    // First event goes through, the second fails and must stay pending.
    let flaky = FailAfter {
        ok: 1,
        seen: Mutex::new(0),
    };
    assert_eq!(relay_once(&h.db, &flaky, 100).await.unwrap(), 1);
    assert_eq!(pending_events(&h).await, 1);

    // Once the sink is back, the remaining event is delivered.
    let sink = Collect::default();
    assert_eq!(relay_once(&h.db, &sink, 100).await.unwrap(), 1);
    assert_eq!(pending_events(&h).await, 0);
    assert_eq!(
        sink.0.lock().unwrap()[0].topic,
        "masterdata.partner.updated"
    );
}
