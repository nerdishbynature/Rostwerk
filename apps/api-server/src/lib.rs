//! Rostwerk API server: wires core, auth and modules into one router.

mod admin;
mod auth;

use std::sync::Arc;

use axum::response::Html;
use axum::routing::get;
use axum::{Json, Router, middleware};
use core_host::{CoreState, ModuleRegistry};
use core_kernel::Db;
use tower_http::trace::TraceLayer;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

pub use auth::AuthConfig;

/// All modules compiled into this binary.
pub fn registry() -> ModuleRegistry {
    ModuleRegistry::new().register(module_masterdata::Masterdata)
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Rostwerk API", description = "API-first ERP core"),
    modifiers(&BearerAuth),
    security(("bearer" = []))
)]
struct ApiDoc;

/// Declares the bearer token scheme so clients (and /docs) can send credentials.
struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some("Development: dev:<tenant-id>:<user>"))
                    .build(),
            ),
        );
    }
}

/// Interactive API docs (Swagger UI from a CDN, reads /openapi.json).
const DOCS_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Rostwerk API</title>
  <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5.17.14/swagger-ui.css">
</head>
<body>
  <div id="swagger-ui"></div>
  <script src="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5.17.14/swagger-ui-bundle.js"></script>
  <script>
    window.ui = SwaggerUIBundle({ url: "/openapi.json", dom_id: "#swagger-ui", persistAuthorization: true });
  </script>
</body>
</html>"##;

/// Builds the full application router.
pub fn app(db: Db, registry: ModuleRegistry, auth: AuthConfig) -> Router {
    let state = CoreState { db };
    let auth = Arc::new(auth);
    let registry = Arc::new(registry);

    let (module_routes, module_api) = registry.router();
    let mut openapi = ApiDoc::openapi();
    openapi.merge(module_api);
    let openapi = Arc::new(openapi);

    // Module routes: authenticate first, then the per-module gate runs.
    let module_routes = module_routes.layer(middleware::from_fn_with_state(
        (state.clone(), auth.clone()),
        auth::authenticate,
    ));

    Router::new()
        .merge(module_routes)
        .nest("/admin", admin::router(registry, auth))
        .route("/health", get(|| async { "ok" }))
        .route("/docs", get(|| async { Html(DOCS_HTML) }))
        .route(
            "/openapi.json",
            get(move || {
                let openapi = openapi.clone();
                async move { Json(openapi.as_ref().clone()) }
            }),
        )
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}
