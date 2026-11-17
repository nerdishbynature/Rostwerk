//! Rostwerk API server: wires core, auth and modules into one router.

mod admin;
mod auth;

use std::sync::Arc;

use axum::routing::get;
use axum::{Json, Router, middleware};
use core_host::{CoreState, ModuleRegistry};
use core_kernel::Db;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;

pub use auth::AuthConfig;

/// All modules compiled into this binary.
pub fn registry() -> ModuleRegistry {
    ModuleRegistry::new().register(module_masterdata::Masterdata)
}

#[derive(OpenApi)]
#[openapi(info(title = "Rostwerk API", description = "API-first ERP core"))]
struct ApiDoc;

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
