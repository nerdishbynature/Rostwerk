//! Rostwerk API server: wires core, auth and modules into one router.

mod admin;
mod auth;

use std::sync::Arc;

use axum::response::Html;
use axum::routing::get;
use axum::{Json, Router, middleware};
use core_host::{CoreState, ModuleRegistry, SystemRole};
use core_kernel::Db;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa::openapi::security::{
    ApiKey, ApiKeyValue, AuthorizationCode, Flow, HttpAuthScheme, HttpBuilder, OAuth2, Scopes,
    SecurityRequirement, SecurityScheme,
};

pub use auth::{AuthConfig, OidcVerifier, TENANT_HEADER};

/// All modules compiled into this binary.
pub fn registry() -> ModuleRegistry {
    ModuleRegistry::new().register(module_masterdata::Masterdata)
}

#[derive(OpenApi)]
#[openapi(info(title = "Rostwerk API", description = "API-first ERP core"))]
struct ApiDoc;

/// Declares how clients (and /docs) send credentials.
///
/// - `bearer`: any bearer token, an OIDC access token or a dev token.
/// - `oidc`: browser sign-in at the issuer (authorization code + PKCE).
/// - `tenant`: the `X-Rostwerk-Tenant` header, for subjects in several tenants.
fn add_security(openapi: &mut utoipa::openapi::OpenApi, auth: &AuthConfig) {
    let components = openapi.components.get_or_insert_with(Default::default);
    let description = match (auth.oidc.is_some(), auth.dev_tokens) {
        (true, true) => "OIDC access token, or dev:<tenant-id>:<user>",
        (true, false) => "OIDC access token",
        (false, _) => "Development: dev:<tenant-id>:<user>",
    };
    components.add_security_scheme(
        "bearer",
        SecurityScheme::Http(
            HttpBuilder::new()
                .scheme(HttpAuthScheme::Bearer)
                .bearer_format("JWT")
                .description(Some(description))
                .build(),
        ),
    );
    let mut alternatives = vec![
        SecurityRequirement::new("bearer", Vec::<String>::new()),
        SecurityRequirement::new("bearer", Vec::<String>::new())
            .add("tenant", Vec::<String>::new()),
    ];
    // Only the authorization code flow: an `openIdConnect` scheme would make
    // Swagger UI offer every grant and scope the issuer advertises.
    if let (Some(verifier), Some(login)) = (&auth.oidc, &auth.docs_login) {
        let flow = AuthorizationCode::new(
            &login.authorization_url,
            &login.token_url,
            Scopes::from_iter([("openid", "Sign in with OpenID Connect")]),
        );
        components.add_security_scheme(
            "oidc",
            SecurityScheme::OAuth2(OAuth2::with_description(
                [Flow::AuthorizationCode(flow)],
                format!("Sign in at {}", verifier.issuer()),
            )),
        );
        alternatives.push(SecurityRequirement::new("oidc", ["openid"]));
        alternatives
            .push(SecurityRequirement::new("oidc", ["openid"]).add("tenant", Vec::<String>::new()));
    }
    components.add_security_scheme(
        "tenant",
        SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
            "X-Rostwerk-Tenant",
            "Tenant id. Needed only if your identity belongs to several tenants.",
        ))),
    );
    openapi.security = Some(alternatives);
}

/// Interactive API docs (Swagger UI from a CDN, reads /openapi.json).
fn docs_html(docs_client_id: Option<&str>) -> String {
    // JSON-encode, so the value cannot break out of the script.
    let client_id = serde_json::to_string(&docs_client_id).expect("serialize client id");
    format!(
        r##"<!doctype html>
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
    const clientId = {client_id};
    window.ui = SwaggerUIBundle({{
      url: "/openapi.json",
      dom_id: "#swagger-ui",
      persistAuthorization: true,
      oauth2RedirectUrl: window.location.origin + "/docs/oauth2-redirect.html",
    }});
    if (clientId) {{
      window.ui.initOAuth({{ clientId, usePkceWithAuthorizationCodeGrant: true, scopes: "openid" }});
    }}
  </script>
</body>
</html>"##
    )
}

/// Swagger UI's OAuth 2 callback page (from swagger-ui-dist 5.17.14, Apache-2.0).
const OAUTH2_REDIRECT_HTML: &str = include_str!("../assets/oauth2-redirect.html");

/// Builds the full application router.
pub fn app(db: Db, registry: ModuleRegistry, auth: AuthConfig, system_role: SystemRole) -> Router {
    let bus = registry.bus(db.clone());
    let state = CoreState {
        db,
        bus,
        ext_entities: registry.ext_entities(),
        system_role,
    };
    let auth = Arc::new(auth);
    let registry = Arc::new(registry);

    let (module_routes, module_api) = registry.router();
    let (core_routes, core_api) = core_host::audit::routes()
        .merge(core_host::ext::routes())
        .split_for_parts();
    let mut openapi = ApiDoc::openapi();
    add_security(&mut openapi, &auth);
    let docs = docs_html(auth.docs_login.as_ref().map(|l| l.client_id.as_str()));
    openapi.merge(core_api);
    openapi.merge(module_api);
    let openapi = Arc::new(openapi);

    // Tenant routes (core + modules): authenticate first, then the per-module gate runs.
    let tenant_routes = core_routes
        .merge(module_routes)
        .layer(middleware::from_fn_with_state(
            (state.clone(), auth.clone()),
            auth::authenticate,
        ));

    Router::new()
        .merge(tenant_routes)
        .nest("/admin", admin::router(registry, auth))
        .route("/health", get(|| async { "ok" }))
        .route("/docs", get(move || async move { Html(docs) }))
        .route(
            "/docs/oauth2-redirect.html",
            get(|| async { Html(OAUTH2_REDIRECT_HTML) }),
        )
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
