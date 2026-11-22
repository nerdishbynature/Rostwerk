//! Dry runs of write endpoints (`Prefer: dry-run`): same code path, rolled back.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{Harness, call, create_tenant, harness, token};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Calls the app and returns status, headers and JSON body.
async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    tenant: &str,
    prefer: Option<&str>,
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {}", token(tenant)))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(prefer) = prefer {
        req = req.header("prefer", prefer);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, serde_json::from_slice(&bytes).unwrap())
}

fn applied(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("preference-applied")
        .map(|v| v.to_str().unwrap())
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

async fn assert_nothing_stored(h: &Harness, tenant: &str) {
    for table in ["partners", "articles", "audit_log", "outbox"] {
        assert_eq!(count(h, table, tenant).await, 0, "{table} must be empty");
    }
}

fn article(number: &str) -> Value {
    json!({
        "number": number,
        "name": "Beratung",
        "kind": "service",
        "unit": "h",
        "sales_price_net": { "amount": "120.00", "currency": "EUR" },
        "tax_category": "standard",
    })
}

#[tokio::test]
async fn dry_run_create_partner_returns_result_and_events_but_stores_nothing() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Dry create", &["masterdata"]).await;

    let (status, headers, body) = send(
        &h.app,
        "POST",
        "/v1/partners",
        &t,
        Some("dry-run"),
        json!({ "name": "Probe GmbH", "kind": "customer" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(applied(&headers), Some("dry-run"));

    let result = &body["result"];
    assert_eq!(result["name"], "Probe GmbH");
    assert_eq!(result["kind"], "customer");
    assert!(result["id"].is_string());
    assert!(result["created_at"].is_string());

    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{body}");
    assert_eq!(events[0]["topic"], "masterdata.partner.created");
    assert_eq!(&events[0]["payload"], result);

    assert_nothing_stored(&h, &t).await;
    let (status, _) = call(
        &h.app,
        "GET",
        &format!("/v1/partners/{}", result["id"].as_str().unwrap()),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dry_run_update_partner_leaves_it_unchanged() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Dry update", &["masterdata"]).await;
    let (status, _, partner) = send(
        &h.app,
        "POST",
        "/v1/partners",
        &t,
        None,
        json!({ "name": "Alt GmbH", "kind": "customer" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let uri = format!("/v1/partners/{}", partner["id"].as_str().unwrap());

    let (status, headers, body) = send(
        &h.app,
        "PATCH",
        &uri,
        &t,
        Some("dry-run"),
        json!({ "name": "Neu GmbH" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(applied(&headers), Some("dry-run"));
    assert_eq!(body["result"]["name"], "Neu GmbH");
    assert_eq!(body["events"][0]["topic"], "masterdata.partner.updated");
    assert_eq!(body["events"][0]["payload"]["name"], "Neu GmbH");

    let (_, stored) = call(&h.app, "GET", &uri, &token(&t), None).await;
    assert_eq!(stored, partner, "partner must be unchanged");
    // Only the real create left traces.
    assert_eq!(count(&h, "audit_log", &t).await, 1);
    assert_eq!(count(&h, "outbox", &t).await, 1);
}

#[tokio::test]
async fn dry_run_errors_match_the_real_call() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Dry errors", &["masterdata"]).await;

    // Validation in Rust.
    let invalid = json!({ "name": "  ", "kind": "customer" });
    let real = send(&h.app, "POST", "/v1/partners", &t, None, invalid.clone()).await;
    let dry = send(&h.app, "POST", "/v1/partners", &t, Some("dry-run"), invalid).await;
    assert_eq!(real.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!((dry.0, &dry.2), (real.0, &real.2));
    assert_eq!(applied(&dry.1), None);

    // Not found.
    let missing = "/v1/partners/00000000-0000-7000-8000-000000000000";
    let real = send(&h.app, "PATCH", missing, &t, None, json!({ "name": "X" })).await;
    let dry = send(
        &h.app,
        "PATCH",
        missing,
        &t,
        Some("dry-run"),
        json!({ "name": "X" }),
    )
    .await;
    assert_eq!(real.0, StatusCode::NOT_FOUND);
    assert_eq!((dry.0, &dry.2), (real.0, &real.2));

    assert_nothing_stored(&h, &t).await;
}

#[tokio::test]
async fn dry_run_runs_database_constraints() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Dry constraints", &["masterdata"]).await;
    let (status, _, _) = send(&h.app, "POST", "/v1/articles", &t, None, article("A-1")).await;
    assert_eq!(status, StatusCode::CREATED);

    // The duplicate is only caught by the unique constraint in Postgres.
    let real = send(&h.app, "POST", "/v1/articles", &t, None, article("A-1")).await;
    let dry = send(
        &h.app,
        "POST",
        "/v1/articles",
        &t,
        Some("dry-run"),
        article("A-1"),
    )
    .await;
    assert_eq!(real.0, StatusCode::CONFLICT);
    assert_eq!((dry.0, &dry.2), (real.0, &real.2));

    // A new number works in a dry run and is still free afterwards.
    let (status, headers, body) = send(
        &h.app,
        "POST",
        "/v1/articles",
        &t,
        Some("dry-run"),
        article("A-2"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(applied(&headers), Some("dry-run"));
    assert_eq!(body["result"]["sales_price_net"]["amount"], "120.0000");
    assert_eq!(body["events"][0]["topic"], "masterdata.article.created");
    assert_eq!(count(&h, "articles", &t).await, 1);
    let (status, _, _) = send(&h.app, "POST", "/v1/articles", &t, None, article("A-2")).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn without_preference_the_response_is_unchanged() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Dry off", &["masterdata"]).await;

    let (status, headers, body) = send(
        &h.app,
        "POST",
        "/v1/partners",
        &t,
        Some("return=minimal"),
        json!({ "name": "Echt GmbH", "kind": "supplier" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(applied(&headers), None);
    assert_eq!(body["name"], "Echt GmbH");
    assert!(body.get("result").is_none());
    assert_eq!(count(&h, "partners", &t).await, 1);
}

/// Guard for new modules: every write endpoint under /v1 must support dry runs,
/// i.e. declare `params(DryRun)` and finish with `DryRun::respond`.
#[tokio::test]
async fn every_write_endpoint_documents_dry_run() {
    let Some(h) = harness().await else { return };
    let (status, spec) = call(&h.app, "GET", "/openapi.json", "", None).await;
    assert_eq!(status, StatusCode::OK);

    let mut checked = 0;
    for (path, item) in spec["paths"].as_object().unwrap() {
        if !path.starts_with("/v1/") {
            continue;
        }
        for method in ["post", "put", "patch", "delete"] {
            let Some(op) = item.get(method) else { continue };
            let has_prefer = op["parameters"].as_array().is_some_and(|params| {
                params
                    .iter()
                    .any(|p| p["name"] == "Prefer" && p["in"] == "header")
            });
            assert!(
                has_prefer,
                "{method} {path} does not support Prefer: dry-run"
            );
            checked += 1;
        }
    }
    assert_eq!(
        checked, 15,
        "partners and articles: create and update; ext fields: put and delete; \
         accounts: create, update, delete; entries: create, update, delete, post, reverse; \
         chart: setup"
    );
}
