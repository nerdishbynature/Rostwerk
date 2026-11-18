//! Articles (products and services) in the master data module.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::{call, create_tenant, harness, token};
use serde_json::{Value, json};

fn article(number: &str) -> Value {
    json!({
        "number": number,
        "name": "Schraube M8",
        "kind": "goods",
        "unit": "pcs",
        "sales_price_net": { "amount": "0.35", "currency": "EUR" },
        "tax_category": "standard",
        "ext": { "color": "silver" },
    })
}

/// A tenant with masterdata and the custom field `color` that [`article`] uses.
async fn tenant(app: &Router, name: &str, modules: &[&str]) -> String {
    let t = create_tenant(app, name, modules).await;
    let (status, body) = call(
        app,
        "PUT",
        "/v1/ext-fields/masterdata.article/color",
        &token(&t),
        Some(json!({ "type": "string", "label": "Color" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    t
}

async fn create(app: &Router, tenant: &str, body: Value) -> (StatusCode, Value) {
    call(app, "POST", "/v1/articles", &token(tenant), Some(body)).await
}

#[tokio::test]
async fn crud() {
    let Some(h) = harness().await else { return };
    let t = tenant(&h.app, "Articles CRUD", &["masterdata"]).await;

    let (status, created) = create(&h.app, &t, article("A-100")).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["number"], "A-100");
    assert_eq!(created["kind"], "goods");
    assert_eq!(created["unit"], "pcs");
    assert_eq!(created["sales_price_net"]["amount"], "0.3500");
    assert_eq!(created["sales_price_net"]["currency"], "EUR");
    assert_eq!(created["tax_category"], "standard");
    assert_eq!(created["active"], true);
    assert_eq!(created["ext"]["color"], "silver");
    let id = created["id"].as_str().unwrap();

    let (status, fetched) = call(
        &h.app,
        "GET",
        &format!("/v1/articles/{id}"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched, created);

    let (status, updated) = call(
        &h.app,
        "PATCH",
        &format!("/v1/articles/{id}"),
        &token(&t),
        Some(json!({
            "name": "Beratung",
            "kind": "service",
            "unit": "h",
            "sales_price_net": { "amount": "120.00", "currency": "EUR" },
            "tax_category": "reduced",
            "active": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["number"], "A-100");
    assert_eq!(updated["name"], "Beratung");
    assert_eq!(updated["kind"], "service");
    assert_eq!(updated["unit"], "h");
    assert_eq!(updated["sales_price_net"]["amount"], "120.0000");
    assert_eq!(updated["tax_category"], "reduced");
    assert_eq!(updated["active"], false);
    assert_eq!(updated["ext"]["color"], "silver");

    let (status, page) = call(&h.app, "GET", "/v1/articles", &token(&t), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"], json!([updated]));
    assert_eq!(page["next_cursor"], Value::Null);

    let missing = uuid::Uuid::now_v7();
    let (status, _) = call(
        &h.app,
        "PATCH",
        &format!("/v1/articles/{missing}"),
        &token(&t),
        Some(json!({ "name": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn emits_events_and_audit() {
    let Some(h) = harness().await else { return };
    let t = tenant(&h.app, "Articles events", &["masterdata"]).await;
    let (_, created) = create(&h.app, &t, article("E-1")).await;
    let id = created["id"].as_str().unwrap();
    let (status, _) = call(
        &h.app,
        "PATCH",
        &format!("/v1/articles/{id}"),
        &token(&t),
        Some(json!({ "active": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let events: Vec<(String, Value)> =
        sqlx::query_as("SELECT topic, payload FROM outbox WHERE tenant_id = $1::uuid AND topic LIKE 'masterdata.%' ORDER BY id")
            .bind(&t)
            .fetch_all(&h.admin_pool)
            .await
            .unwrap();
    let topics: Vec<_> = events.iter().map(|(topic, _)| topic.as_str()).collect();
    assert_eq!(
        topics,
        ["masterdata.article.created", "masterdata.article.updated"]
    );
    assert_eq!(events[0].1["id"], created["id"]);
    assert_eq!(events[1].1["active"], false);

    let (status, audit) = call(
        &h.app,
        "GET",
        &format!("/v1/audit?entity_id={id}"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = audit["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{audit}");
    assert_eq!(items[0]["entity_type"], "articles");
    assert_eq!(items[0]["action"], "insert");
    assert_eq!(items[1]["action"], "update");
}

#[tokio::test]
async fn duplicate_number_conflicts_only_within_tenant() {
    let Some(h) = harness().await else { return };
    let a = tenant(&h.app, "Articles dup A", &["masterdata"]).await;
    let b = tenant(&h.app, "Articles dup B", &["masterdata"]).await;

    let (status, _) = create(&h.app, &a, article("D-1")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = create(&h.app, &a, article("D-1")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "conflict");

    // Another tenant may use the same number.
    let (status, _) = create(&h.app, &b, article("D-1")).await;
    assert_eq!(status, StatusCode::CREATED);

    // Renaming onto an existing number conflicts too.
    let (_, other) = create(&h.app, &a, article("D-2")).await;
    let id = other["id"].as_str().unwrap();
    let (status, _) = call(
        &h.app,
        "PATCH",
        &format!("/v1/articles/{id}"),
        &token(&a),
        Some(json!({ "number": "D-1" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn tenants_are_isolated() {
    let Some(h) = harness().await else { return };
    let a = tenant(&h.app, "Articles iso A", &["masterdata"]).await;
    let b = tenant(&h.app, "Articles iso B", &["masterdata"]).await;
    let (_, created) = create(&h.app, &a, article("I-1")).await;
    let id = created["id"].as_str().unwrap();

    let uri = format!("/v1/articles/{id}");
    let (status, _) = call(&h.app, "GET", &uri, &token(&b), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &h.app,
        "PATCH",
        &uri,
        &token(&b),
        Some(json!({ "name": "stolen" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, page) = call(&h.app, "GET", "/v1/articles", &token(&b), None).await;
    assert_eq!(page["items"], json!([]));

    // Raw query as the app role, outside any tenant transaction: RLS shows nothing.
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM articles")
        .fetch_one(&h.app_pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    let (_, fetched) = call(&h.app, "GET", &uri, &token(&a), None).await;
    assert_eq!(fetched["name"], "Schraube M8");
}

#[tokio::test]
async fn paginates_with_cursor() {
    let Some(h) = harness().await else { return };
    let t = tenant(&h.app, "Articles paging", &["masterdata"]).await;
    for i in 0..5 {
        let (status, _) = create(&h.app, &t, article(&format!("P-{i}"))).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let mut numbers = Vec::new();
    let mut uri = "/v1/articles?limit=2".to_string();
    loop {
        let (status, page) = call(&h.app, "GET", &uri, &token(&t), None).await;
        assert_eq!(status, StatusCode::OK);
        for item in page["items"].as_array().unwrap() {
            numbers.push(item["number"].as_str().unwrap().to_string());
        }
        match page["next_cursor"].as_str() {
            Some(cursor) => uri = format!("/v1/articles?limit=2&after={cursor}"),
            None => break,
        }
    }
    assert_eq!(
        numbers,
        (0..5).map(|i| format!("P-{i}")).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn rejects_invalid_input() {
    let Some(h) = harness().await else { return };
    let t = tenant(&h.app, "Articles invalid", &["masterdata"]).await;

    let mut cases = Vec::new();
    for (field, value) in [
        ("number", json!("  ")),
        ("name", json!("")),
        ("unit", json!("")),
        ("ext", json!([1, 2])),
        (
            "sales_price_net",
            json!({ "amount": "-1", "currency": "EUR" }),
        ),
        (
            "sales_price_net",
            json!({ "amount": "1.23456", "currency": "EUR" }),
        ),
    ] {
        let mut body = article("V-1");
        body[field] = value;
        cases.push(body);
    }
    for body in cases {
        let (status, res) = create(&h.app, &t, body.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body} -> {res}");
    }

    // Unknown enum values and bad currency codes fail to deserialize.
    for (field, value) in [
        ("kind", json!("rental")),
        ("tax_category", json!("super")),
        (
            "sales_price_net",
            json!({ "amount": "1", "currency": "eur" }),
        ),
    ] {
        let mut body = article("V-2");
        body[field] = value;
        let (status, _) = create(&h.app, &t, body.clone()).await;
        assert!(status.is_client_error(), "{body} -> {status}");
    }
}

#[tokio::test]
async fn listed_in_openapi() {
    let Some(h) = harness().await else { return };
    let (status, spec) = call(&h.app, "GET", "/openapi.json", "", None).await;
    assert_eq!(status, StatusCode::OK);
    let articles = &spec["paths"]["/v1/articles"];
    assert!(articles["get"].is_object() && articles["post"].is_object());
    let one = &spec["paths"]["/v1/articles/{id}"];
    assert!(one["get"].is_object() && one["patch"].is_object());
    assert!(spec["components"]["schemas"]["Article"].is_object());
    assert!(spec["components"]["schemas"]["Money"].is_object());
}
