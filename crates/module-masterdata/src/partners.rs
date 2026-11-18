use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use chrono::{DateTime, Utc};
use core_host::{CoreState, DryRun, ExtSchema};
use core_kernel::{Error, Result, TenantCtx, TenantTx, new_id};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Extensible entity name, for custom fields.
pub const ENTITY: &str = "masterdata.partner";

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

#[derive(Debug, Serialize, ToSchema)]
pub struct Partner {
    pub id: Uuid,
    pub name: String,
    pub kind: PartnerKind,
    pub email: Option<String>,
    /// Custom fields of the tenant, see `GET /v1/ext-fields/masterdata.partner/schema`.
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
    /// Custom fields, validated against `GET /v1/ext-fields/masterdata.partner/schema`.
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
    /// Cursor: return partners after this id, in the requested order.
    pub after: Option<Uuid>,
    /// Page size, 1 to 500. Default 50.
    pub limit: Option<i64>,
    /// Case-insensitive substring of name or email.
    pub q: Option<String>,
    /// Only partners of this kind; `customer` and `supplier` include `both`.
    pub kind: Option<PartnerKind>,
    /// `created` (default), `name`, `email` or `kind`; prefix `-` for descending.
    pub sort: Option<String>,
}

/// Sort key column and direction of a partner list.
fn sort_order(sort: Option<&str>) -> Result<(&'static str, bool)> {
    let sort = sort.unwrap_or("created");
    let (field, desc) = match sort.strip_prefix('-') {
        Some(field) => (field, true),
        None => (sort, false),
    };
    let field = match field {
        "created" => "created",
        "name" => "name",
        "email" => "email",
        "kind" => "kind",
        _ => {
            return Err(Error::Validation(
                "sort must be created, name, email or kind, optionally prefixed with -".into(),
            ));
        }
    };
    Ok((field, desc))
}

/// List partners, cursor-paginated. Ordered by creation time unless `sort` says otherwise;
/// `q` and `kind` filter.
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
    let (sort, desc) = sort_order(params.sort.as_deref())?;
    let q = params.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    // Keyset pagination over (sort key, id): the cursor stays a partner id.
    // Ties on the sort key are broken by id in the same direction.
    let mut items = sqlx::query_as!(
        Partner,
        r#"WITH keyed AS (
             SELECT p.*,
                    CASE $3::text
                        WHEN 'name' THEN lower(p.name)
                        WHEN 'email' THEN lower(coalesce(p.email, ''))
                        WHEN 'kind' THEN p.kind
                        ELSE ''
                    END AS sort_key
             FROM partners p
           ),
           cursor AS (SELECT sort_key, id FROM keyed WHERE id = $1)
         SELECT k.id AS "id!", k.name AS "name!", k.kind AS "kind!: PartnerKind", k.email,
                k.ext AS "ext!", k.created_at AS "created_at!", k.updated_at AS "updated_at!"
         FROM keyed k
         WHERE ($1::uuid IS NULL
                OR ($4 AND (k.sort_key, k.id) < (SELECT sort_key, id FROM cursor))
                OR (NOT $4 AND (k.sort_key, k.id) > (SELECT sort_key, id FROM cursor)))
           AND ($5::text IS NULL
                OR k.name ILIKE '%' || $5 || '%'
                OR k.email ILIKE '%' || $5 || '%')
           AND ($6::text IS NULL OR k.kind = $6 OR k.kind = 'both')
         ORDER BY CASE WHEN $4 THEN NULL ELSE k.sort_key END ASC,
                  CASE WHEN $4 THEN k.sort_key END DESC,
                  CASE WHEN $4 THEN NULL ELSE k.id END ASC,
                  CASE WHEN $4 THEN k.id END DESC
         LIMIT $2"#,
        params.after,
        limit + 1,
        sort,
        desc,
        q,
        params.kind as Option<PartnerKind>,
    )
    .fetch_all(tx.conn())
    .await?;
    let ext = ExtSchema::load(&mut tx, ENTITY).await?;
    tx.commit().await?;
    for partner in &mut items {
        partner.ext = ext.visible(std::mem::take(&mut partner.ext));
    }

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
    params(DryRun),
    request_body = NewPartner,
    responses((status = 201, body = Partner), (status = 422, description = "Invalid input"))
)]
pub async fn create(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    dry_run: DryRun,
    Json(new): Json<NewPartner>,
) -> Result<Response> {
    let name = new.name.trim();
    if name.is_empty() {
        return Err(Error::Validation("name must not be empty".into()));
    }
    let ext = new.ext.unwrap_or_else(|| Value::Object(Default::default()));

    let mut tx = ctx.begin(&state.db).await?;
    let schema = ExtSchema::load(&mut tx, ENTITY).await?;
    schema.validate(&ext)?;
    let mut partner = sqlx::query_as!(
        Partner,
        r#"INSERT INTO partners (id, tenant_id, name, kind, email, ext)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, name, kind AS "kind: PartnerKind", email, ext, created_at, updated_at"#,
        new_id(),
        tx.tenant_id().0,
        name,
        new.kind as PartnerKind,
        new.email,
        ext,
    )
    .fetch_one(tx.conn())
    .await?;
    partner.ext = schema.visible(partner.ext);
    tx.publish("masterdata.partner.created", &partner).await?;
    dry_run.respond(tx, StatusCode::CREATED, partner).await
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PartnerChanges {
    pub name: Option<String>,
    pub kind: Option<PartnerKind>,
    /// Empty string removes the email.
    pub email: Option<String>,
    /// Replaces all custom fields; validated against
    /// `GET /v1/ext-fields/masterdata.partner/schema`.
    #[schema(value_type = Option<Object>)]
    pub ext: Option<Value>,
}

/// Change a partner. Fields left out stay as they are.
#[utoipa::path(
    patch,
    path = "/v1/partners/{id}",
    tag = "masterdata",
    params(("id" = Uuid, Path, description = "Partner id"), DryRun),
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
    dry_run: DryRun,
    Json(changes): Json<PartnerChanges>,
) -> Result<Response> {
    let name = changes.name.as_deref().map(str::trim);
    if name.is_some_and(str::is_empty) {
        return Err(Error::Validation("name must not be empty".into()));
    }
    // None = keep, Some(None) = clear, Some(Some(x)) = set.
    let email = changes
        .email
        .map(|e| Some(e.trim().to_string()).filter(|e| !e.is_empty()));

    let mut tx = ctx.begin(&state.db).await?;
    let schema = ExtSchema::load(&mut tx, ENTITY).await?;
    if let Some(ext) = &changes.ext {
        schema.validate(ext)?;
    }
    let mut partner = sqlx::query_as!(
        Partner,
        r#"UPDATE partners SET
             name = coalesce($2, name),
             kind = coalesce($3, kind),
             email = CASE WHEN $4 THEN $5 ELSE email END,
             ext = coalesce($6, ext),
             updated_at = now()
         WHERE id = $1
         RETURNING id, name, kind AS "kind: PartnerKind", email, ext, created_at, updated_at"#,
        id,
        name,
        changes.kind as Option<PartnerKind>,
        email.is_some(),
        email.flatten(),
        changes.ext,
    )
    .fetch_optional(tx.conn())
    .await?
    .ok_or(Error::NotFound)?;
    partner.ext = schema.visible(partner.ext);
    tx.publish("masterdata.partner.updated", &partner).await?;
    dry_run.respond(tx, StatusCode::OK, partner).await
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
    let partner = find(&mut tx, id).await?;
    tx.commit().await?;

    partner.map(Json).ok_or(Error::NotFound)
}

/// Loads one partner of the transaction's tenant, with its current custom fields.
pub(crate) async fn find(tx: &mut TenantTx, id: Uuid) -> Result<Option<Partner>> {
    let partner = sqlx::query_as!(
        Partner,
        r#"SELECT id, name, kind AS "kind: PartnerKind", email, ext, created_at, updated_at
         FROM partners WHERE id = $1"#,
        id,
    )
    .fetch_optional(tx.conn())
    .await?;
    let Some(partner) = partner else {
        return Ok(None);
    };
    let schema = ExtSchema::load(tx, ENTITY).await?;
    Ok(Some(Partner {
        ext: schema.visible(partner.ext),
        ..partner
    }))
}
