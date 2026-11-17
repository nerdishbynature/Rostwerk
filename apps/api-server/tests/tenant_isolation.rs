//! End-to-end proof that tenants cannot see each other's data.
//!
//! Needs the Postgres from docker-compose. Reads DATABASE_URL and
//! DATABASE_ADMIN_URL (from the environment or `.env`); skips if they are unset.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use core_kernel::Db;
use http_body_util::BodyExt;
use rostwerk_api::{AuthConfig, app, registry};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const ADMIN: &str = "test-admin";

struct Harness {
    app: Router,
    app_pool: PgPool,
}

async fn harness() -> Option<Harness> {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env")).ok();
    let (Ok(url), Ok(admin_url)) = (
        std::env::var("DATABASE_URL"),
        std::env::var("DATABASE_ADMIN_URL"),
    ) else {
        eprintln!("skipping: DATABASE_URL / DATABASE_ADMIN_URL not set");
        return None;
    };

    let admin_pool = PgPool::connect(&admin_url).await.expect("connect admin");
    registry().migrate(&admin_pool).await.expect("migrate");

    let app_pool = PgPool::connect(&url).await.expect("connect app");
    let auth = AuthConfig {
        dev_tokens: true,
        admin_token: Some(ADMIN.into()),
    };
    let app = app(Db::new(app_pool.clone()), registry(), auth);
    Some(Harness { app, app_pool })
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    let body = match body {
        Some(json) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn create_tenant(app: &Router, name: &str, modules: &[&str]) -> String {
    let (status, body) = call(
        app,
        "POST",
        "/admin/tenants",
        ADMIN,
        Some(json!({ "name": name, "modules": modules })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_string()
}

fn token(tenant: &str) -> String {
    format!("dev:{tenant}:tester")
}

#[tokio::test]
async fn tenants_are_isolated() {
    let Some(h) = harness().await else { return };
    let a = create_tenant(&h.app, "Tenant A", &["masterdata"]).await;
    let b = create_tenant(&h.app, "Tenant B", &["masterdata"]).await;

    let (status, partner) = call(
        &h.app,
        "POST",
        "/v1/partners",
        &token(&a),
        Some(json!({ "name": "ACME GmbH", "kind": "customer", "ext": { "segment": "A" } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{partner}");
    let id = partner["id"].as_str().unwrap();

    // A sees its partner.
    let (status, _) = call(
        &h.app,
        "GET",
        &format!("/v1/partners/{id}"),
        &token(&a),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // B does not, neither by id nor in the list.
    let (status, _) = call(
        &h.app,
        "GET",
        &format!("/v1/partners/{id}"),
        &token(&b),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, page) = call(&h.app, "GET", "/v1/partners", &token(&b), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"], json!([]));
}

#[tokio::test]
async fn rls_hides_rows_without_tenant_context() {
    let Some(h) = harness().await else { return };
    let a = create_tenant(&h.app, "Tenant RLS", &["masterdata"]).await;
    let (status, _) = call(
        &h.app,
        "POST",
        "/v1/partners",
        &token(&a),
        Some(json!({ "name": "Hidden AG", "kind": "supplier" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Raw query as the app role, outside any tenant transaction: RLS shows nothing.
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM partners")
        .fetch_one(&h.app_pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    // Writing into another tenant's name is rejected by the policy.
    let foreign = sqlx::query(
        "INSERT INTO partners (id, tenant_id, name, kind) VALUES (gen_random_uuid(), $1::uuid, 'x', 'both')",
    )
    .bind(&a)
    .execute(&h.app_pool)
    .await;
    assert!(foreign.is_err(), "insert without tenant context must fail");
}

#[tokio::test]
async fn disabled_module_is_invisible() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Tenant without modules", &[]).await;

    let (status, _) = call(&h.app, "GET", "/v1/partners", &token(&t), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Switch the module on: now it answers.
    let (status, _) = call(
        &h.app,
        "PUT",
        &format!("/admin/tenants/{t}/modules"),
        ADMIN,
        Some(json!(["masterdata"])),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&h.app, "GET", "/v1/partners", &token(&t), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn rejects_missing_or_unknown_credentials() {
    let Some(h) = harness().await else { return };
    let (status, _) = call(&h.app, "GET", "/v1/partners", "garbage", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let unknown = uuid::Uuid::now_v7();
    let (status, _) = call(
        &h.app,
        "GET",
        "/v1/partners",
        &token(&unknown.to_string()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn paginates_with_cursor() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Tenant paging", &["masterdata"]).await;
    for i in 0..5 {
        let (status, _) = call(
            &h.app,
            "POST",
            "/v1/partners",
            &token(&t),
            Some(json!({ "name": format!("Partner {i}"), "kind": "both" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let mut names = Vec::new();
    let mut uri = "/v1/partners?limit=2".to_string();
    loop {
        let (status, page) = call(&h.app, "GET", &uri, &token(&t), None).await;
        assert_eq!(status, StatusCode::OK);
        for item in page["items"].as_array().unwrap() {
            names.push(item["name"].as_str().unwrap().to_string());
        }
        match page["next_cursor"].as_str() {
            Some(cursor) => uri = format!("/v1/partners?limit=2&after={cursor}"),
            None => break,
        }
    }
    assert_eq!(
        names,
        (0..5).map(|i| format!("Partner {i}")).collect::<Vec<_>>()
    );
}
