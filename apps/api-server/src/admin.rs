//! Operator API: create tenants, switch modules on and off, admit members.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use core_host::members::{self, Member, MemberKind};
use core_host::tenants::{self, NewTenant, Tenant};
use core_host::{CoreState, ModuleManifest, ModuleRegistry};
use core_kernel::{Error, Result, TenantId};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::{AuthConfig, bearer};

pub(crate) fn router(registry: Arc<ModuleRegistry>, auth: Arc<AuthConfig>) -> Router<CoreState> {
    let manifests = Arc::new(registry.manifests());
    Router::new()
        .route("/tenants", post(create_tenant))
        .route("/tenants/{id}/modules", put(set_modules))
        .route("/tenants/{id}/members", get(list_members))
        .route(
            "/tenants/{id}/members/{subject}",
            put(put_member).delete(delete_member),
        )
        .route(
            "/modules",
            get(move || {
                let manifests = manifests.clone();
                async move { Json(manifests.as_ref().clone()) as Json<Vec<ModuleManifest>> }
            }),
        )
        .layer(axum::Extension(registry))
        .layer(middleware::from_fn_with_state(auth, require_admin))
}

async fn require_admin(
    State(auth): State<Arc<AuthConfig>>,
    req: Request,
    next: Next,
) -> Result<Response> {
    let expected = auth.admin_token.as_deref().ok_or(Error::NotFound)?;
    if bearer(&req) != Some(expected) {
        return Err(Error::Unauthorized);
    }
    Ok(next.run(req).await)
}

fn check_modules(registry: &ModuleRegistry, modules: &[String]) -> Result<()> {
    match modules.iter().find(|m| !registry.is_known(m)) {
        Some(unknown) => Err(Error::Validation(format!("unknown module: {unknown}"))),
        None => Ok(()),
    }
}

async fn create_tenant(
    State(state): State<CoreState>,
    axum::Extension(registry): axum::Extension<Arc<ModuleRegistry>>,
    Json(new): Json<NewTenant>,
) -> Result<(StatusCode, Json<Tenant>)> {
    if new.name.trim().is_empty() {
        return Err(Error::Validation("name must not be empty".into()));
    }
    check_modules(&registry, &new.modules)?;
    let tenant = tenants::create(&state.db, new).await?;
    Ok((StatusCode::CREATED, Json(tenant)))
}

async fn set_modules(
    State(state): State<CoreState>,
    axum::Extension(registry): axum::Extension<Arc<ModuleRegistry>>,
    Path(id): Path<Uuid>,
    Json(modules): Json<Vec<String>>,
) -> Result<Json<Tenant>> {
    check_modules(&registry, &modules)?;
    tenants::set_modules(&state.db, TenantId(id), &modules)
        .await?
        .map(Json)
        .ok_or(Error::NotFound)
}

async fn list_members(
    State(state): State<CoreState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<Member>>> {
    tenants::find(&state.db, TenantId(id))
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(members::list(&state.db, TenantId(id)).await?))
}

#[derive(Debug, Deserialize)]
struct MemberBody {
    kind: MemberKind,
}

/// Admits an OIDC subject (`sub` claim) to a tenant as user or agent.
async fn put_member(
    State(state): State<CoreState>,
    Path((id, subject)): Path<(Uuid, String)>,
    Json(body): Json<MemberBody>,
) -> Result<Json<Member>> {
    if subject.trim().is_empty() {
        return Err(Error::Validation("subject must not be empty".into()));
    }
    members::upsert(&state.db, TenantId(id), &subject, body.kind)
        .await?
        .map(Json)
        .ok_or(Error::NotFound)
}

async fn delete_member(
    State(state): State<CoreState>,
    Path((id, subject)): Path<(Uuid, String)>,
) -> Result<StatusCode> {
    if members::remove(&state.db, TenantId(id), &subject).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::NotFound)
    }
}
