//! Shared test harness. Needs the Postgres from docker-compose; reads
//! DATABASE_URL and DATABASE_ADMIN_URL (environment or `.env`), else tests skip.
#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use core_kernel::Db;
use http_body_util::BodyExt;
use rostwerk_api::{AuthConfig, app, registry};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

pub const ADMIN: &str = "test-admin";

pub struct Harness {
    pub app: Router,
    pub app_pool: PgPool,
    /// Owner role: bypasses RLS, for inspecting rows in tests.
    pub admin_pool: PgPool,
    pub db: Db,
}

pub async fn harness() -> Option<Harness> {
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
    let db = Db::new(app_pool.clone());
    let app = app(db.clone(), registry(), auth);
    Some(Harness {
        app,
        app_pool,
        admin_pool,
        db,
    })
}

pub async fn call(
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

pub async fn create_tenant(app: &Router, name: &str, modules: &[&str]) -> String {
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

pub fn token(tenant: &str) -> String {
    format!("dev:{tenant}:tester")
}
