use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use core_host::CoreState;
use core_kernel::{Error, Result, TenantCtx, new_id};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum PartnerKind {
    Customer,
    Supplier,
    Both,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct Partner {
    pub id: Uuid,
    pub name: String,
    pub kind: PartnerKind,
    pub email: Option<String>,
    /// Custom fields of the tenant.
    #[schema(value_type = Object)]
    pub ext: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct NewPartner {
    pub name: String,
    pub kind: PartnerKind,
    pub email: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub ext: Option<Value>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PartnerPage {
    pub items: Vec<Partner>,
    /// Pass as `after` to get the next page. Absent on the last page.
    pub next_cursor: Option<Uuid>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListParams {
    /// Cursor: return partners after this id.
    pub after: Option<Uuid>,
    /// Page size, 1 to 500. Default 50.
    pub limit: Option<i64>,
}

const COLUMNS: &str = "id, name, kind, email, ext, created_at, updated_at";

/// List partners, ordered by id (creation time), cursor-paginated.
#[utoipa::path(
    get,
    path = "/v1/partners",
    tag = "masterdata",
    params(ListParams),
    responses((status = 200, body = PartnerPage))
)]
pub async fn list(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Query(params): Query<ListParams>,
) -> Result<Json<PartnerPage>> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(Error::Validation(format!("limit must be 1 to {MAX_LIMIT}")));
    }

    let mut tx = ctx.begin(&state.db).await?;
    // One extra row tells whether another page exists.
    let mut items = sqlx::query_as::<_, Partner>(&format!(
        "SELECT {COLUMNS} FROM partners
         WHERE ($1::uuid IS NULL OR id > $1)
         ORDER BY id
         LIMIT $2"
    ))
    .bind(params.after)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let next_cursor = if items.len() as i64 > limit {
        items.truncate(limit as usize);
        items.last().map(|p| p.id)
    } else {
        None
    };
    Ok(Json(PartnerPage { items, next_cursor }))
}

/// Create a partner.
#[utoipa::path(
    post,
    path = "/v1/partners",
    tag = "masterdata",
    request_body = NewPartner,
    responses((status = 201, body = Partner), (status = 422, description = "Invalid input"))
)]
pub async fn create(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Json(new): Json<NewPartner>,
) -> Result<(StatusCode, Json<Partner>)> {
    let name = new.name.trim();
    if name.is_empty() {
        return Err(Error::Validation("name must not be empty".into()));
    }
    let ext = new.ext.unwrap_or_else(|| Value::Object(Default::default()));
    if !ext.is_object() {
        return Err(Error::Validation("ext must be a JSON object".into()));
    }

    let mut tx = ctx.begin(&state.db).await?;
    let partner = sqlx::query_as::<_, Partner>(&format!(
        "INSERT INTO partners (id, tenant_id, name, kind, email, ext)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING {COLUMNS}"
    ))
    .bind(new_id())
    .bind(tx.tenant_id())
    .bind(name)
    .bind(new.kind)
    .bind(new.email)
    .bind(ext)
    .fetch_one(tx.conn())
    .await?;
    tx.publish("masterdata.partner.created", &partner).await?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(partner)))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PartnerChanges {
    pub name: Option<String>,
    pub kind: Option<PartnerKind>,
    /// Empty string removes the email.
    pub email: Option<String>,
    /// Replaces all custom fields.
    #[schema(value_type = Option<Object>)]
    pub ext: Option<Value>,
}

/// Change a partner. Fields left out stay as they are.
#[utoipa::path(
    patch,
    path = "/v1/partners/{id}",
    tag = "masterdata",
    params(("id" = Uuid, Path, description = "Partner id")),
    request_body = PartnerChanges,
    responses(
        (status = 200, body = Partner),
        (status = 404, description = "Not found"),
        (status = 422, description = "Invalid input")
    )
)]
pub async fn update(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(id): Path<Uuid>,
    Json(changes): Json<PartnerChanges>,
) -> Result<Json<Partner>> {
    let name = changes.name.as_deref().map(str::trim);
    if name.is_some_and(str::is_empty) {
        return Err(Error::Validation("name must not be empty".into()));
    }
    if changes.ext.as_ref().is_some_and(|ext| !ext.is_object()) {
        return Err(Error::Validation("ext must be a JSON object".into()));
    }
    // None = keep, Some(None) = clear, Some(Some(x)) = set.
    let email = changes
        .email
        .map(|e| Some(e.trim().to_string()).filter(|e| !e.is_empty()));

    let mut tx = ctx.begin(&state.db).await?;
    let partner = sqlx::query_as::<_, Partner>(&format!(
        "UPDATE partners SET
             name = coalesce($2, name),
             kind = coalesce($3, kind),
             email = CASE WHEN $4 THEN $5 ELSE email END,
             ext = coalesce($6, ext),
             updated_at = now()
         WHERE id = $1
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(name)
    .bind(changes.kind)
    .bind(email.is_some())
    .bind(email.flatten())
    .bind(changes.ext)
    .fetch_optional(tx.conn())
    .await?
    .ok_or(Error::NotFound)?;
    tx.publish("masterdata.partner.updated", &partner).await?;
    tx.commit().await?;

    Ok(Json(partner))
}

/// Get one partner.
#[utoipa::path(
    get,
    path = "/v1/partners/{id}",
    tag = "masterdata",
    params(("id" = Uuid, Path, description = "Partner id")),
    responses((status = 200, body = Partner), (status = 404, description = "Not found"))
)]
pub async fn get(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(id): Path<Uuid>,
) -> Result<Json<Partner>> {
    let mut tx = ctx.begin(&state.db).await?;
    let partner =
        sqlx::query_as::<_, Partner>(&format!("SELECT {COLUMNS} FROM partners WHERE id = $1"))
            .bind(id)
            .fetch_optional(tx.conn())
            .await?;
    tx.commit().await?;

    partner.map(Json).ok_or(Error::NotFound)
}
