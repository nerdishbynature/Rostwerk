//! Read access to the audit log. Writing happens only through database triggers.

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use core_kernel::{Error, Result, TenantCtx};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::CoreState;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct AuditEntry {
    pub id: i64,
    pub occurred_at: DateTime<Utc>,
    /// `user:<id>`, `agent:<id>` or `db:<role>` for changes outside the API.
    pub actor: String,
    pub entity_type: String,
    pub entity_id: Uuid,
    /// `insert`, `update` or `delete`.
    pub action: String,
    #[schema(value_type = Option<Object>)]
    pub before: Option<Value>,
    #[schema(value_type = Option<Object>)]
    pub after: Option<Value>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AuditPage {
    pub items: Vec<AuditEntry>,
    pub next_cursor: Option<i64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AuditParams {
    /// Filter by entity type (table), e.g. `partners`.
    pub entity_type: Option<String>,
    /// Filter by entity id.
    pub entity_id: Option<Uuid>,
    /// Cursor: entries after this id.
    pub after: Option<i64>,
    /// Page size, 1 to 500. Default 50.
    pub limit: Option<i64>,
}

/// Core routes for the audit log. Available to every tenant, not module-gated.
pub fn routes() -> OpenApiRouter<CoreState> {
    OpenApiRouter::new().routes(routes!(list))
}

/// List audit entries of the tenant, oldest first.
#[utoipa::path(
    get,
    path = "/v1/audit",
    tag = "core",
    params(AuditParams),
    responses((status = 200, body = AuditPage))
)]
pub async fn list(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Query(params): Query<AuditParams>,
) -> Result<Json<AuditPage>> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(Error::Validation(format!("limit must be 1 to {MAX_LIMIT}")));
    }

    let mut tx = ctx.begin(&state.db).await?;
    let mut items = sqlx::query_as!(
        AuditEntry,
        "SELECT id, occurred_at, actor, entity_type, entity_id, action, before, after
         FROM audit_log
         WHERE ($1::text IS NULL OR entity_type = $1)
           AND ($2::uuid IS NULL OR entity_id = $2)
           AND ($3::bigint IS NULL OR id > $3)
         ORDER BY id
         LIMIT $4",
        params.entity_type,
        params.entity_id,
        params.after,
        limit + 1,
    )
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let next_cursor = if items.len() as i64 > limit {
        items.truncate(limit as usize);
        items.last().map(|e| e.id)
    } else {
        None
    };
    Ok(Json(AuditPage { items, next_cursor }))
}
