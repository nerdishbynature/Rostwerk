//! Outbox relay to NATS JetStream.
//!
//! Needs the NATS from docker-compose (`NATS_URL`, environment or `.env`) in
//! addition to the database. Without it the tests skip, unless
//! ROSTWERK_REQUIRE_NATS=1 (set this in CI), then they fail.

mod common;

use std::time::Duration;

use async_nats::jetstream::{self, consumer};
use axum::http::StatusCode;
use common::{call, create_tenant, harness, token};
use core_host::outbox::{Event, EventSink, relay_once};
use event_nats::{NatsSink, STREAM};
use futures_util::StreamExt;
use serde_json::{Value, json};

async fn jetstream() -> Option<jetstream::Context> {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env")).ok();
    let Ok(url) = std::env::var("NATS_URL") else {
        if std::env::var("ROSTWERK_REQUIRE_NATS").is_ok_and(|v| v == "1") {
            panic!("ROSTWERK_REQUIRE_NATS=1 but NATS_URL not set");
        }
        eprintln!("skipping: NATS_URL not set");
        return None;
    };
    let client = async_nats::connect(&url).await.expect("connect to NATS");
    Some(jetstream::new(client))
}

/// All messages on `subject` currently in the stream, as JSON.
async fn messages(js: &jetstream::Context, subject: &str) -> Vec<(Value, Option<String>)> {
    let stream = js.get_stream(STREAM).await.expect("stream exists");
    let consumer = stream
        .create_consumer(consumer::pull::Config {
            filter_subject: subject.into(),
            deliver_policy: consumer::DeliverPolicy::All,
            ack_policy: consumer::AckPolicy::None,
            ..Default::default()
        })
        .await
        .expect("create consumer");
    let mut batch = consumer
        .fetch()
        .max_messages(100)
        .expires(Duration::from_secs(1))
        .messages()
        .await
        .expect("fetch");
    let mut out = Vec::new();
    while let Some(msg) = batch.next().await {
        let msg = msg.expect("message");
        let id = msg
            .headers
            .as_ref()
            .and_then(|h| h.get("Nats-Msg-Id"))
            .map(|v| v.to_string());
        out.push((serde_json::from_slice(&msg.payload).expect("json"), id));
    }
    out
}

#[tokio::test]
async fn relay_publishes_partner_created_to_jetstream() {
    let Some(js) = jetstream().await else { return };
    let Some(h) = harness().await else { return };

    let sink = NatsSink::new(js.clone());
    sink.ensure_stream().await.unwrap();
    // Idempotent: a second start finds the stream and keeps it.
    sink.ensure_stream().await.unwrap();

    let t = create_tenant(&h.app, "NATS A", &["masterdata"]).await;
    let (status, partner) = call(
        &h.app,
        "POST",
        "/v1/partners",
        &token(&t),
        Some(json!({ "name": "Strom GmbH", "kind": "customer" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{partner}");

    assert_eq!(relay_once(&h.db, &sink, 100).await.unwrap(), 1);

    let subject = format!("rostwerk.{t}.masterdata.partner.created");
    let got = messages(&js, &subject).await;
    assert_eq!(got.len(), 1, "{got:?}");
    let (event, msg_id) = &got[0];
    assert_eq!(event["tenant_id"], t);
    assert_eq!(event["topic"], "masterdata.partner.created");
    assert_eq!(event["actor"], "user:tester");
    assert_eq!(event["payload"]["id"], partner["id"]);
    assert_eq!(event["payload"]["name"], "Strom GmbH");
    assert_eq!(msg_id.as_deref(), event["id"].as_str());
}

#[tokio::test]
async fn redelivered_event_is_deduplicated() {
    let Some(js) = jetstream().await else { return };
    let Some(h) = harness().await else { return };
    let sink = NatsSink::new(js.clone());
    sink.ensure_stream().await.unwrap();

    let t = create_tenant(&h.app, "NATS dedupe", &[]).await;
    let ctx = core_kernel::TenantCtx::new(
        core_kernel::TenantId(t.parse().unwrap()),
        core_kernel::Actor::User("tester".into()),
        Vec::<String>::new(),
    );
    let mut tx = ctx.begin(&h.db).await.unwrap();
    let id = tx
        .publish("test.thing.happened", &json!({ "x": 1 }))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let event: Event = sqlx::query_as(
        "SELECT id, tenant_id, topic, payload, actor, occurred_at FROM outbox WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&h.admin_pool)
    .await
    .unwrap();

    // The relay crashed after the publish but before marking the row: same event twice.
    sink.publish(&event).await.unwrap();
    sink.publish(&event).await.unwrap();

    let got = messages(&js, &format!("rostwerk.{t}.test.thing.happened")).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0["payload"], json!({ "x": 1 }));
}
