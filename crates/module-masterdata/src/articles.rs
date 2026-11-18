use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use core_host::CoreState;
use core_kernel::{Currency, Error, Money, Result, TenantCtx, new_id};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

/// Name of the unique constraint on (tenant_id, number), see the migration.
const NUMBER_CONSTRAINT: &str = "articles_tenant_id_number_key";
/// Prices are stored as numeric(19, 4).
const PRICE_SCALE: u32 = 4;
const PRICE_INTEGER_DIGITS: u32 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum ArticleKind {
    Goods,
    Service,
}

/// VAT rate category. The actual rate depends on the country and date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum TaxCategory {
    Standard,
    Reduced,
    Zero,
}

/// OpenAPI shape of [`Money`].
#[derive(ToSchema)]
#[schema(as = Money)]
#[allow(dead_code)]
struct MoneySchema {
    /// Decimal amount as a string, e.g. `"19.99"`.
    #[schema(example = "19.99")]
    amount: String,
    /// ISO 4217 currency code.
    #[schema(example = "EUR", pattern = "^[A-Z]{3}$")]
    currency: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Article {
    pub id: Uuid,
    /// Article number, unique within the tenant.
    pub number: String,
    pub name: String,
    pub kind: ArticleKind,
    /// Unit of measure, e.g. `pcs`, `h`, `kg`.
    pub unit: String,
    /// Net sales price per unit.
    #[schema(value_type = MoneySchema)]
    pub sales_price_net: Money,
    pub tax_category: TaxCategory,
    pub active: bool,
    /// Custom fields of the tenant.
    #[schema(value_type = Object)]
    pub ext: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// An `articles` row as stored; the price is split into amount and currency.
/// Queries read into this and convert with [`ArticleRow::into_article`].
struct ArticleRow {
    id: Uuid,
    number: String,
    name: String,
    kind: ArticleKind,
    unit: String,
    sales_price_amount: Decimal,
    sales_price_currency: String,
    tax_category: TaxCategory,
    active: bool,
    ext: Value,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl ArticleRow {
    fn into_article(self) -> sqlx::Result<Article> {
        let currency: Currency =
            self.sales_price_currency
                .parse()
                .map_err(|err| sqlx::Error::ColumnDecode {
                    index: "sales_price_currency".into(),
                    source: Box::new(err),
                })?;
        Ok(Article {
            id: self.id,
            number: self.number,
            name: self.name,
            kind: self.kind,
            unit: self.unit,
            sales_price_net: Money::new(self.sales_price_amount, currency),
            tax_category: self.tax_category,
            active: self.active,
            ext: self.ext,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct NewArticle {
    pub number: String,
    pub name: String,
    pub kind: ArticleKind,
    pub unit: String,
    #[schema(value_type = MoneySchema)]
    pub sales_price_net: Money,
    pub tax_category: TaxCategory,
    /// Default `true`.
    pub active: Option<bool>,
    #[schema(value_type = Option<Object>)]
    pub ext: Option<Value>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ArticlePage {
    pub items: Vec<Article>,
    /// Pass as `after` to get the next page. Absent on the last page.
    pub next_cursor: Option<Uuid>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ArticleListParams {
    /// Cursor: return articles after this id.
    pub after: Option<Uuid>,
    /// Page size, 1 to 500. Default 50.
    pub limit: Option<i64>,
}

fn non_empty<'a>(field: &str, value: &'a str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        return Err(Error::Validation(format!("{field} must not be empty")));
    }
    Ok(value)
}

fn check_price(price: &Money) -> Result<()> {
    let amount = price.amount;
    if amount.is_sign_negative() && !amount.is_zero() {
        return Err(Error::Validation(
            "sales_price_net must not be negative".into(),
        ));
    }
    if amount.normalize().scale() > PRICE_SCALE {
        return Err(Error::Validation(format!(
            "sales_price_net has more than {PRICE_SCALE} decimal places"
        )));
    }
    if amount.trunc() >= Decimal::from(10_i64.pow(PRICE_INTEGER_DIGITS)) {
        return Err(Error::Validation("sales_price_net is too large".into()));
    }
    Ok(())
}

fn check_ext(ext: &Value) -> Result<()> {
    if !ext.is_object() {
        return Err(Error::Validation("ext must be a JSON object".into()));
    }
    Ok(())
}

/// Turns a duplicate article number into 409, other errors pass through.
fn number_conflict(err: sqlx::Error) -> Error {
    if let Some(db) = err.as_database_error()
        && db.constraint() == Some(NUMBER_CONSTRAINT)
    {
        return Error::Conflict("article number already exists".into());
    }
    err.into()
}

/// List articles, ordered by id (creation time), cursor-paginated.
#[utoipa::path(
    get,
    path = "/v1/articles",
    tag = "masterdata",
    params(ArticleListParams),
    responses((status = 200, body = ArticlePage))
)]
pub async fn list(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Query(params): Query<ArticleListParams>,
) -> Result<Json<ArticlePage>> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(Error::Validation(format!("limit must be 1 to {MAX_LIMIT}")));
    }

    let mut tx = ctx.begin(&state.db).await?;
    // One extra row tells whether another page exists.
    let mut items = sqlx::query_as!(
        ArticleRow,
        r#"SELECT id, number, name, kind AS "kind: ArticleKind", unit, sales_price_amount,
                   sales_price_currency, tax_category AS "tax_category: TaxCategory", active,
                   ext, created_at, updated_at
         FROM articles
         WHERE ($1::uuid IS NULL OR id > $1)
         ORDER BY id
         LIMIT $2"#,
        params.after,
        limit + 1,
    )
    .fetch_all(tx.conn())
    .await?
    .into_iter()
    .map(ArticleRow::into_article)
    .collect::<sqlx::Result<Vec<_>>>()?;
    tx.commit().await?;

    let next_cursor = if items.len() as i64 > limit {
        items.truncate(limit as usize);
        items.last().map(|a| a.id)
    } else {
        None
    };
    Ok(Json(ArticlePage { items, next_cursor }))
}

/// Create an article.
#[utoipa::path(
    post,
    path = "/v1/articles",
    tag = "masterdata",
    request_body = NewArticle,
    responses(
        (status = 201, body = Article),
        (status = 409, description = "Article number already exists"),
        (status = 422, description = "Invalid input")
    )
)]
pub async fn create(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Json(new): Json<NewArticle>,
) -> Result<(StatusCode, Json<Article>)> {
    let number = non_empty("number", &new.number)?;
    let name = non_empty("name", &new.name)?;
    let unit = non_empty("unit", &new.unit)?;
    check_price(&new.sales_price_net)?;
    let ext = new.ext.unwrap_or_else(|| Value::Object(Default::default()));
    check_ext(&ext)?;

    let mut tx = ctx.begin(&state.db).await?;
    let article = sqlx::query_as!(
        ArticleRow,
        r#"INSERT INTO articles (id, tenant_id, number, name, kind, unit, sales_price_amount,
                               sales_price_currency, tax_category, active, ext)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         RETURNING id, number, name, kind AS "kind: ArticleKind", unit, sales_price_amount,
                   sales_price_currency, tax_category AS "tax_category: TaxCategory", active,
                   ext, created_at, updated_at"#,
        new_id(),
        tx.tenant_id().0,
        number,
        name,
        new.kind as ArticleKind,
        unit,
        new.sales_price_net.amount,
        new.sales_price_net.currency.as_str(),
        new.tax_category as TaxCategory,
        new.active.unwrap_or(true),
        ext,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(number_conflict)?
    .into_article()?;
    tx.publish("masterdata.article.created", &article).await?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(article)))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ArticleChanges {
    pub number: Option<String>,
    pub name: Option<String>,
    pub kind: Option<ArticleKind>,
    pub unit: Option<String>,
    #[schema(value_type = Option<MoneySchema>)]
    pub sales_price_net: Option<Money>,
    pub tax_category: Option<TaxCategory>,
    pub active: Option<bool>,
    /// Replaces all custom fields.
    #[schema(value_type = Option<Object>)]
    pub ext: Option<Value>,
}

/// Change an article. Fields left out stay as they are.
#[utoipa::path(
    patch,
    path = "/v1/articles/{id}",
    tag = "masterdata",
    params(("id" = Uuid, Path, description = "Article id")),
    request_body = ArticleChanges,
    responses(
        (status = 200, body = Article),
        (status = 404, description = "Not found"),
        (status = 409, description = "Article number already exists"),
        (status = 422, description = "Invalid input")
    )
)]
pub async fn update(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(id): Path<Uuid>,
    Json(changes): Json<ArticleChanges>,
) -> Result<Json<Article>> {
    let number = changes
        .number
        .as_deref()
        .map(|n| non_empty("number", n))
        .transpose()?;
    let name = changes
        .name
        .as_deref()
        .map(|n| non_empty("name", n))
        .transpose()?;
    let unit = changes
        .unit
        .as_deref()
        .map(|u| non_empty("unit", u))
        .transpose()?;
    if let Some(price) = &changes.sales_price_net {
        check_price(price)?;
    }
    if let Some(ext) = &changes.ext {
        check_ext(ext)?;
    }

    let mut tx = ctx.begin(&state.db).await?;
    let article = sqlx::query_as!(
        ArticleRow,
        r#"UPDATE articles SET
             number = coalesce($2, number),
             name = coalesce($3, name),
             kind = coalesce($4, kind),
             unit = coalesce($5, unit),
             sales_price_amount = coalesce($6, sales_price_amount),
             sales_price_currency = coalesce($7, sales_price_currency),
             tax_category = coalesce($8, tax_category),
             active = coalesce($9, active),
             ext = coalesce($10, ext),
             updated_at = now()
         WHERE id = $1
         RETURNING id, number, name, kind AS "kind: ArticleKind", unit, sales_price_amount,
                   sales_price_currency, tax_category AS "tax_category: TaxCategory", active,
                   ext, created_at, updated_at"#,
        id,
        number,
        name,
        changes.kind as Option<ArticleKind>,
        unit,
        changes.sales_price_net.map(|p| p.amount),
        changes.sales_price_net.map(|p| p.currency.to_string()),
        changes.tax_category as Option<TaxCategory>,
        changes.active,
        changes.ext,
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(number_conflict)?
    .ok_or(Error::NotFound)?
    .into_article()?;
    tx.publish("masterdata.article.updated", &article).await?;
    tx.commit().await?;

    Ok(Json(article))
}

/// Get one article.
#[utoipa::path(
    get,
    path = "/v1/articles/{id}",
    tag = "masterdata",
    params(("id" = Uuid, Path, description = "Article id")),
    responses((status = 200, body = Article), (status = 404, description = "Not found"))
)]
pub async fn get(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(id): Path<Uuid>,
) -> Result<Json<Article>> {
    let mut tx = ctx.begin(&state.db).await?;
    let article = sqlx::query_as!(
        ArticleRow,
        r#"SELECT id, number, name, kind AS "kind: ArticleKind", unit, sales_price_amount,
                   sales_price_currency, tax_category AS "tax_category: TaxCategory", active,
                   ext, created_at, updated_at
         FROM articles WHERE id = $1"#,
        id,
    )
    .fetch_optional(tx.conn())
    .await?
    .map(ArticleRow::into_article)
    .transpose()?;
    tx.commit().await?;

    article.map(Json).ok_or(Error::NotFound)
}
