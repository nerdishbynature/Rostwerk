//! Shared test harness.
//!
//! Every test gets its own freshly migrated database (`rostwerk_test_<uuid>`),
//! dropped again when the [`Harness`] goes out of scope. Tests therefore never
//! see each other's rows, nor those of a running dev server.
//!
//! Needs the Postgres from docker-compose. Connection settings come from
//! DATABASE_URL and DATABASE_ADMIN_URL (environment or `.env`); only server,
//! port and roles are used, the database name is replaced. Without them the
//! tests skip, unless ROSTWERK_REQUIRE_DB=1 (set this in CI), then they fail.
#![allow(dead_code)]

pub mod idp;

use std::str::FromStr;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use core_kernel::Db;
use http_body_util::BodyExt;
use rostwerk_api::{AuthConfig, app, registry};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};
use tower::ServiceExt;

pub const ADMIN: &str = "test-admin";

pub struct Harness {
    pub app: Router,
    /// App role, subject to RLS.
    pub app_pool: PgPool,
    /// Owner role: bypasses RLS, for inspecting rows in tests.
    pub admin_pool: PgPool,
    pub db: Db,
    /// Dropped last: removes the database.
    _database: TestDatabase,
}

pub async fn harness() -> Option<Harness> {
    harness_with(AuthConfig {
        dev_tokens: true,
        admin_token: Some(ADMIN.into()),
        ..Default::default()
    })
    .await
}

/// Like [`harness`], with the given auth configuration.
pub async fn harness_with(auth: AuthConfig) -> Option<Harness> {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env")).ok();
    let (Ok(url), Ok(admin_url)) = (
        std::env::var("DATABASE_URL"),
        std::env::var("DATABASE_ADMIN_URL"),
    ) else {
        if std::env::var("ROSTWERK_REQUIRE_DB").is_ok_and(|v| v == "1") {
            panic!("ROSTWERK_REQUIRE_DB=1 but DATABASE_URL / DATABASE_ADMIN_URL not set");
        }
        eprintln!("skipping: DATABASE_URL / DATABASE_ADMIN_URL not set");
        return None;
    };
    let app_opts = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    let admin_opts = PgConnectOptions::from_str(&admin_url).expect("parse DATABASE_ADMIN_URL");

    let database = TestDatabase::create(&admin_opts, app_opts.get_username()).await;

    let admin_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(admin_opts.database(&database.name))
        .await
        .expect("connect admin to test database");
    registry().migrate(&admin_pool).await.expect("migrate");

    let app_pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(app_opts.database(&database.name))
        .await
        .expect("connect app to test database");
    let db = Db::new(app_pool.clone());
    let app = app(db.clone(), registry(), auth);
    Some(Harness {
        app,
        app_pool,
        admin_pool,
        db,
        _database: database,
    })
}

/// A throwaway database, set up like docker/postgres/init does for `rostwerk`.
struct TestDatabase {
    name: String,
    /// Admin connection to the original database, used to create and drop.
    maintenance: PgConnectOptions,
}

impl TestDatabase {
    async fn create(admin: &PgConnectOptions, app_role: &str) -> Self {
        let name = format!("rostwerk_test_{}", uuid::Uuid::now_v7().simple());
        let admin_role = admin.get_username().to_string();

        let mut conn = PgConnection::connect_with(admin)
            .await
            .expect("connect admin");
        sqlx::raw_sql(&format!(r#"CREATE DATABASE "{name}""#))
            .execute(&mut conn)
            .await
            .expect("create test database");
        conn.close().await.ok();

        // Grants and default privileges are per database, so repeat them here.
        let mut conn = PgConnection::connect_with(&admin.clone().database(&name))
            .await
            .expect("connect test database");
        sqlx::raw_sql(&format!(
            r#"GRANT CONNECT ON DATABASE "{name}" TO "{app_role}";
               GRANT USAGE ON SCHEMA public TO "{app_role}";
               ALTER DEFAULT PRIVILEGES FOR ROLE "{admin_role}" IN SCHEMA public
                   GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO "{app_role}";"#
        ))
        .execute(&mut conn)
        .await
        .expect("grant app role");
        conn.close().await.ok();

        Self {
            name,
            maintenance: admin.clone(),
        }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        // Drop is sync and may run inside the test's runtime: use a fresh thread.
        let name = std::mem::take(&mut self.name);
        let opts = self.maintenance.clone();
        let result = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async move {
                    let mut conn = PgConnection::connect_with(&opts).await?;
                    // FORCE ends the pools' connections that are still open.
                    sqlx::raw_sql(&format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#))
                        .execute(&mut conn)
                        .await?;
                    conn.close().await?;
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
                })
        })
        .join();
        if !matches!(result, Ok(Ok(()))) {
            eprintln!("warning: could not drop test database");
        }
    }
}

pub async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    call_with(app, method, uri, token, &[], body).await
}

/// Like [`call`], with extra request headers.
pub async fn call_with(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
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
