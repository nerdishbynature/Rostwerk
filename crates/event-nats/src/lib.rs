//! [`EventSink`] that publishes outbox events to NATS JetStream.
//!
//! Each event goes to the subject `rostwerk.<tenant_id>.<topic>`, e.g.
//! `rostwerk.0193….masterdata.partner.created`, as the JSON-serialized [`Event`]
//! (id, tenant, topic, payload, actor, time). The outbox event id is sent as
//! `Nats-Msg-Id`, so JetStream drops redeliveries of the same event within the
//! stream's duplicate window. The relay delivers at least once; this makes the
//! stream effectively exactly-once for retries that happen inside that window.
//!
//! Kept out of `core-host` so the core does not depend on a NATS client.

use anyhow::Context;
use async_nats::HeaderMap;
use async_nats::jetstream::{self, stream};
use core_host::outbox::{Event, EventSink, SinkFuture};

/// Name of the stream that holds all Rostwerk events.
pub const STREAM: &str = "ROSTWERK";
/// Prefix of every event subject.
pub const SUBJECT_PREFIX: &str = "rostwerk";

/// The subject an event is published on: `rostwerk.<tenant_id>.<topic>`.
pub fn subject(event: &Event) -> String {
    format!("{SUBJECT_PREFIX}.{}.{}", event.tenant_id, event.topic)
}

/// Publishes events to JetStream and waits for the server's ack.
pub struct NatsSink {
    js: jetstream::Context,
}

impl NatsSink {
    /// Connects to `url` and creates the stream [`STREAM`] if it is missing.
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let client = async_nats::connect(url)
            .await
            .with_context(|| format!("connect to NATS at {url}"))?;
        let sink = Self::new(jetstream::new(client));
        sink.ensure_stream().await?;
        Ok(sink)
    }

    /// Wraps an existing JetStream context. Does not create the stream.
    pub fn new(js: jetstream::Context) -> Self {
        Self { js }
    }

    /// Creates the stream [`STREAM`] with subjects `rostwerk.>` if it does not exist.
    /// An existing stream is left as it is.
    pub async fn ensure_stream(&self) -> anyhow::Result<()> {
        self.js
            .get_or_create_stream(stream::Config {
                name: STREAM.into(),
                subjects: vec![format!("{SUBJECT_PREFIX}.>")],
                ..Default::default()
            })
            .await
            .with_context(|| format!("create JetStream stream {STREAM}"))?;
        Ok(())
    }
}

impl EventSink for NatsSink {
    fn publish<'a>(&'a self, event: &'a Event) -> SinkFuture<'a> {
        Box::pin(async move {
            let body = serde_json::to_vec(event)?;
            let mut headers = HeaderMap::new();
            headers.insert("Nats-Msg-Id", event.id.to_string().as_str());

            // First await sends, second waits for the stream's ack. Either failing
            // is an error, so the relay keeps the event and retries it.
            let ack = self
                .js
                .publish_with_headers(subject(event), headers, body.into())
                .await
                .context("publish to JetStream")?
                .await
                .context("JetStream did not ack")?;
            if ack.duplicate {
                tracing::debug!(id = %event.id, "event already in stream, deduplicated");
            }
            Ok(())
        })
    }
}
