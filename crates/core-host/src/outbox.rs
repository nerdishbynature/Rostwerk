//! Outbox relay: moves committed events from the `outbox` table to an [`EventSink`].
//!
//! Delivery is at-least-once: an event is marked published only after the sink
//! accepted it. Consumers must be idempotent (dedupe on the event id).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use core_kernel::{Db, Result};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// A committed domain event.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Event {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub topic: String,
    pub payload: Value,
    pub actor: String,
    pub occurred_at: DateTime<Utc>,
}

pub type SinkFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

/// Where events go: NATS JetStream (`event-nats`), the log, a collector in tests.
pub trait EventSink: Send + Sync {
    fn publish<'a>(&'a self, event: &'a Event) -> SinkFuture<'a>;
}

/// Writes events to the tracing log.
pub struct LogSink;

impl EventSink for LogSink {
    fn publish<'a>(&'a self, event: &'a Event) -> SinkFuture<'a> {
        Box::pin(async move {
            tracing::info!(
                id = %event.id,
                tenant = %event.tenant_id,
                topic = %event.topic,
                actor = %event.actor,
                "event published"
            );
            Ok(())
        })
    }
}

/// Publishes up to `batch` pending events. Returns how many were published.
///
/// Rows are locked with `SKIP LOCKED`, so several relays can run in parallel.
/// If the sink fails, the transaction rolls back and the batch is retried later.
pub async fn relay_once(db: &Db, sink: &dyn EventSink, batch: i64) -> Result<usize> {
    let mut tx = db.begin_system().await?;
    let events = sqlx::query_as!(
        Event,
        "SELECT id, tenant_id, topic, payload, actor, occurred_at
         FROM outbox
         WHERE published_at IS NULL
         ORDER BY id
         LIMIT $1
         FOR UPDATE SKIP LOCKED",
        batch,
    )
    .fetch_all(tx.conn())
    .await?;

    let mut published = Vec::with_capacity(events.len());
    for event in &events {
        match sink.publish(event).await {
            Ok(()) => published.push(event.id),
            Err(err) => {
                tracing::warn!(id = %event.id, error = %err, "event sink failed, will retry");
                break;
            }
        }
    }

    if !published.is_empty() {
        sqlx::query!(
            "UPDATE outbox SET published_at = now(), attempts = attempts + 1 WHERE id = ANY($1)",
            &published,
        )
        .execute(tx.conn())
        .await?;
    }
    tx.commit().await?;
    Ok(published.len())
}

/// Runs the relay forever. Drains quickly while there is backlog, then polls.
pub async fn run_relay(db: Db, sink: Arc<dyn EventSink>, poll: Duration) {
    const BATCH: i64 = 100;
    loop {
        match relay_once(&db, sink.as_ref(), BATCH).await {
            Ok(n) if n as i64 == BATCH => continue,
            Ok(_) => {}
            Err(err) => tracing::error!(error = %err, "outbox relay failed"),
        }
        tokio::time::sleep(poll).await;
    }
}
