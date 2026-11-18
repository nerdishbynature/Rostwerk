//! Chart of accounts.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use chrono::{DateTime, NaiveDate, Utc};
use core_host::{CoreState, DryRun};
use core_kernel::{Error, Money, Result, TenantCtx, TenantTx, new_id};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::{
    MoneySchema, PageParams, currency_column, non_empty, paginate, violated_constraint,
};

/// Unique constraint on (tenant_id, number), see the migration.
const NUMBER_CONSTRAINT: &str = "accounts_tenant_id_number_key";
/// Foreign key from journal lines to accounts.
const LINE_ACCOUNT_FKEY: &str = "journal_lines_account_fkey";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum AccountType {
    Asset,
    Liability,
    Equity,
    Revenue,
    Expense,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Account {
    pub id: Uuid,
    /// Account number, unique within the tenant.
    pub number: String,
    pub name: String,
    #[serde(rename = "type")]
    #[schema(rename = "type")]
    pub account_type: AccountType,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct NewAccount {
    pub number: String,
    pub name: String,
    #[serde(rename = "type")]
    #[schema(rename = "type")]
    pub account_type: AccountType,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AccountChanges {
    /// Cannot be changed once the account has posted lines.
    pub number: Option<String>,
    pub name: Option<String>,
    /// Cannot be changed once the account has posted lines.
    #[serde(rename = "type")]
    #[schema(rename = "type")]
    pub account_type: Option<AccountType>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AccountPage {
    pub items: Vec<Account>,
    /// Pass as `after` to get the next page. Absent on the last page.
    pub next_cursor: Option<Uuid>,
}

fn number_conflict(err: sqlx::Error) -> Error {
    if violated_constraint(&err) == Some(NUMBER_CONSTRAINT) {
        return Error::Conflict("account number already exists".into());
    }
    err.into()
}

/// Ids of `ids` that are not accounts of the tenant.
pub(crate) async fn missing(tx: &mut TenantTx, ids: &[Uuid]) -> Result<Vec<Uuid>> {
    let found = sqlx::query_scalar!("SELECT id FROM accounts WHERE id = ANY($1)", ids)
        .fetch_all(tx.conn())
        .await?;
    let mut missing: Vec<Uuid> = ids
        .iter()
        .copied()
        .filter(|id| !found.contains(id))
        .collect();
    missing.sort();
    missing.dedup();
    Ok(missing)
}

async fn find(tx: &mut TenantTx, id: Uuid) -> Result<Option<Account>> {
    Ok(sqlx::query_as!(
        Account,
        r#"SELECT id, number, name, type AS "account_type: AccountType", created_at, updated_at
         FROM accounts WHERE id = $1"#,
        id,
    )
    .fetch_optional(tx.conn())
    .await?)
}

/// List the chart of accounts, ordered by id (creation time), cursor-paginated.
#[utoipa::path(
    get,
    path = "/v1/accounting/accounts",
    tag = "accounting",
    params(PageParams),
    responses((status = 200, body = AccountPage))
)]
pub async fn list(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Query(params): Query<PageParams>,
) -> Result<Json<AccountPage>> {
    let limit = params.limit()?;
    let mut tx = ctx.begin(&state.db).await?;
    // One extra row tells whether another page exists.
    let mut items = sqlx::query_as!(
        Account,
        r#"SELECT id, number, name, type AS "account_type: AccountType", created_at, updated_at
         FROM accounts
         WHERE ($1::uuid IS NULL OR id > $1)
         ORDER BY id
         LIMIT $2"#,
        params.after,
        limit + 1,
    )
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let next_cursor = paginate(&mut items, limit, |a| a.id);
    Ok(Json(AccountPage { items, next_cursor }))
}

/// Create an account.
#[utoipa::path(
    post,
    path = "/v1/accounting/accounts",
    tag = "accounting",
    params(DryRun),
    request_body = NewAccount,
    responses(
        (status = 201, body = Account),
        (status = 409, description = "Account number already exists"),
        (status = 422, description = "Invalid input")
    )
)]
pub async fn create(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    dry_run: DryRun,
    Json(new): Json<NewAccount>,
) -> Result<Response> {
    let number = non_empty("number", &new.number)?;
    let name = non_empty("name", &new.name)?;

    let mut tx = ctx.begin(&state.db).await?;
    let account = sqlx::query_as!(
        Account,
        r#"INSERT INTO accounts (id, tenant_id, number, name, type)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id, number, name, type AS "account_type: AccountType", created_at, updated_at"#,
        new_id(),
        tx.tenant_id().0,
        number,
        name,
        new.account_type as AccountType,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(number_conflict)?;
    tx.publish("accounting.account.created", &account).await?;
    dry_run.respond(tx, StatusCode::CREATED, account).await
}

/// Get one account.
#[utoipa::path(
    get,
    path = "/v1/accounting/accounts/{id}",
    tag = "accounting",
    params(("id" = Uuid, Path, description = "Account id")),
    responses((status = 200, body = Account), (status = 404, description = "Not found"))
)]
pub async fn get(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(id): Path<Uuid>,
) -> Result<Json<Account>> {
    let mut tx = ctx.begin(&state.db).await?;
    let account = find(&mut tx, id).await?;
    tx.commit().await?;
    account.map(Json).ok_or(Error::NotFound)
}

/// Change an account. Fields left out stay as they are. Number and type are
/// fixed once the account has posted lines.
#[utoipa::path(
    patch,
    path = "/v1/accounting/accounts/{id}",
    tag = "accounting",
    params(("id" = Uuid, Path, description = "Account id"), DryRun),
    request_body = AccountChanges,
    responses(
        (status = 200, body = Account),
        (status = 404, description = "Not found"),
        (status = 409, description = "Account number already exists, or number or type of an account with postings"),
        (status = 422, description = "Invalid input")
    )
)]
pub async fn update(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(id): Path<Uuid>,
    dry_run: DryRun,
    Json(changes): Json<AccountChanges>,
) -> Result<Response> {
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

    let mut tx = ctx.begin(&state.db).await?;
    // Lock the account, so that its postings cannot change under the check.
    let current = sqlx::query!(
        r#"SELECT number, type AS "account_type: AccountType",
                  EXISTS (SELECT 1 FROM journal_lines l
                            JOIN journal_entries e ON e.id = l.entry_id
                           WHERE l.account_id = a.id AND e.status = 'posted') AS "posted!"
         FROM accounts a WHERE id = $1 FOR UPDATE"#,
        id,
    )
    .fetch_optional(tx.conn())
    .await?
    .ok_or(Error::NotFound)?;
    let renumbered = number.is_some_and(|n| n != current.number);
    let retyped = changes
        .account_type
        .is_some_and(|t| t != current.account_type);
    if current.posted && (renumbered || retyped) {
        return Err(Error::Conflict(
            "number and type of an account with postings cannot be changed".into(),
        ));
    }

    let account = sqlx::query_as!(
        Account,
        r#"UPDATE accounts SET
             number = coalesce($2, number),
             name = coalesce($3, name),
             type = coalesce($4, type),
             updated_at = now()
         WHERE id = $1
         RETURNING id, number, name, type AS "account_type: AccountType", created_at, updated_at"#,
        id,
        number,
        name,
        changes.account_type as Option<AccountType>,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(number_conflict)?;
    tx.publish("accounting.account.updated", &account).await?;
    dry_run.respond(tx, StatusCode::OK, account).await
}

/// Delete an account that no journal entry uses. Returns the deleted account.
#[utoipa::path(
    delete,
    path = "/v1/accounting/accounts/{id}",
    tag = "accounting",
    params(("id" = Uuid, Path, description = "Account id"), DryRun),
    responses(
        (status = 200, body = Account, description = "The deleted account"),
        (status = 404, description = "Not found"),
        (status = 409, description = "The account is used by journal entries")
    )
)]
pub async fn delete(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(id): Path<Uuid>,
    dry_run: DryRun,
) -> Result<Response> {
    let mut tx = ctx.begin(&state.db).await?;
    let account = sqlx::query_as!(
        Account,
        r#"DELETE FROM accounts WHERE id = $1
         RETURNING id, number, name, type AS "account_type: AccountType", created_at, updated_at"#,
        id,
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(|err| {
        if violated_constraint(&err) == Some(LINE_ACCOUNT_FKEY) {
            return Error::Conflict("account is used by journal entries".into());
        }
        err.into()
    })?
    .ok_or(Error::NotFound)?;
    tx.publish("accounting.account.deleted", &account).await?;
    dry_run.respond(tx, StatusCode::OK, account).await
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BalanceParams {
    /// Include postings booked on or before this date. Default: all.
    pub until: Option<NaiveDate>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AccountBalance {
    pub account_id: Uuid,
    pub until: Option<NaiveDate>,
    /// One balance per currency the account has postings in, ordered by
    /// currency. Empty if there are none.
    pub balances: Vec<CurrencyBalance>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CurrencyBalance {
    /// Sum of posted debit lines.
    #[schema(value_type = MoneySchema)]
    pub debit: Money,
    /// Sum of posted credit lines.
    #[schema(value_type = MoneySchema)]
    pub credit: Money,
    /// Debit minus credit: positive is a debit balance, negative a credit balance.
    #[schema(value_type = MoneySchema)]
    pub balance: Money,
}

/// Balance of an account from its posted lines. Drafts do not count.
#[utoipa::path(
    get,
    path = "/v1/accounting/accounts/{id}/balance",
    tag = "accounting",
    params(("id" = Uuid, Path, description = "Account id"), BalanceParams),
    responses((status = 200, body = AccountBalance), (status = 404, description = "Not found"))
)]
pub async fn balance(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(id): Path<Uuid>,
    Query(params): Query<BalanceParams>,
) -> Result<Json<AccountBalance>> {
    let mut tx = ctx.begin(&state.db).await?;
    if find(&mut tx, id).await?.is_none() {
        return Err(Error::NotFound);
    }
    let rows = sqlx::query!(
        r#"SELECT l.currency,
                  coalesce(sum(l.amount) FILTER (WHERE l.side = 'debit'), 0) AS "debit!: Decimal",
                  coalesce(sum(l.amount) FILTER (WHERE l.side = 'credit'), 0) AS "credit!: Decimal"
         FROM journal_lines l
         JOIN journal_entries e ON e.id = l.entry_id
         WHERE l.account_id = $1
           AND e.status = 'posted'
           AND ($2::date IS NULL OR e.booking_date <= $2)
         GROUP BY l.currency
         ORDER BY l.currency"#,
        id,
        params.until,
    )
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let balances = rows
        .into_iter()
        .map(|row| {
            let currency = currency_column("currency", &row.currency)?;
            Ok(CurrencyBalance {
                debit: Money::new(row.debit, currency),
                credit: Money::new(row.credit, currency),
                balance: Money::new(row.debit - row.credit, currency),
            })
        })
        .collect::<sqlx::Result<Vec<_>>>()?;
    Ok(Json(AccountBalance {
        account_id: id,
        until: params.until,
        balances,
    }))
}
