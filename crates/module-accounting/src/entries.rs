//! Journal entries: drafts, posting and reversal.

use std::collections::HashMap;

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
    MoneySchema, check_amount, currency_column, non_empty, page_limit, paginate,
    violated_constraint,
};
use crate::tax::{self, TaxedLine};

/// At most one reversal per entry, see the migration.
const REVERSES_CONSTRAINT: &str = "journal_entries_reverses_id_key";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum EntryStatus {
    /// Editable, not part of any balance.
    Draft,
    /// Numbered and final. Cannot be changed or deleted, only reversed.
    Posted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Side {
    Debit,
    Credit,
}

impl Side {
    fn opposite(self) -> Self {
        match self {
            Side::Debit => Side::Credit,
            Side::Credit => Side::Debit,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Line {
    pub id: Uuid,
    /// Position within the entry, from 1.
    pub line_no: i32,
    pub account_id: Uuid,
    pub side: Side,
    /// Positive amount booked on `side`.
    #[schema(value_type = MoneySchema)]
    pub amount: Money,
    /// Tax code of the line. On an automatic account always its code.
    pub tax_code: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Entry {
    pub id: Uuid,
    pub status: EntryStatus,
    /// Gapless per tenant, assigned when the entry is posted.
    pub sequence_no: Option<i64>,
    pub booking_date: Option<NaiveDate>,
    /// Receipt or document the booking is based on.
    pub document_ref: Option<String>,
    pub description: String,
    /// On a reversal entry: the entry it reverses.
    pub reverses_id: Option<Uuid>,
    /// On a reversed entry: its reversal entry.
    pub reversed_by_id: Option<Uuid>,
    pub posted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub lines: Vec<Line>,
}

struct EntryRow {
    id: Uuid,
    status: EntryStatus,
    sequence_no: Option<i64>,
    booking_date: Option<NaiveDate>,
    document_ref: Option<String>,
    description: String,
    reverses_id: Option<Uuid>,
    reversed_by_id: Option<Uuid>,
    posted_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl EntryRow {
    fn with_lines(self, lines: Vec<Line>) -> Entry {
        Entry {
            id: self.id,
            status: self.status,
            sequence_no: self.sequence_no,
            booking_date: self.booking_date,
            document_ref: self.document_ref,
            description: self.description,
            reverses_id: self.reverses_id,
            reversed_by_id: self.reversed_by_id,
            posted_at: self.posted_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
            lines,
        }
    }
}

struct LineRow {
    id: Uuid,
    entry_id: Uuid,
    line_no: i32,
    account_id: Uuid,
    side: Side,
    amount: Decimal,
    currency: String,
    tax_code: Option<String>,
}

impl LineRow {
    fn into_line(self) -> sqlx::Result<Line> {
        Ok(Line {
            id: self.id,
            line_no: self.line_no,
            account_id: self.account_id,
            side: self.side,
            amount: Money::new(self.amount, currency_column("currency", &self.currency)?),
            tax_code: self.tax_code,
        })
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct NewLine {
    pub account_id: Uuid,
    pub side: Side,
    /// Positive amount, at most 4 decimal places.
    #[schema(value_type = MoneySchema)]
    pub amount: Money,
    /// Optional tax code (see `GET /v1/accounting/tax-codes`). Output codes fit
    /// revenue accounts, input codes expense and asset accounts. On an
    /// automatic account it defaults to, and must equal, the account's code.
    #[serde(default)]
    pub tax_code: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct NewEntry {
    /// Required for posting.
    pub booking_date: Option<NaiveDate>,
    /// Required for posting.
    pub document_ref: Option<String>,
    /// Default empty.
    pub description: Option<String>,
    /// In order; numbered from 1. May be empty while drafting.
    #[serde(default)]
    pub lines: Vec<NewLine>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct EntryChanges {
    pub booking_date: Option<NaiveDate>,
    pub document_ref: Option<String>,
    pub description: Option<String>,
    /// Replaces all lines.
    pub lines: Option<Vec<NewLine>>,
}

#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct Reversal {
    /// Default: today.
    pub booking_date: Option<NaiveDate>,
    /// Default: the document reference of the reversed entry.
    pub document_ref: Option<String>,
    /// Default: `Storno of #<sequence_no>`, followed by the reversed entry's description.
    pub description: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EntryPage {
    pub items: Vec<Entry>,
    /// Pass as `after` to get the next page. Absent on the last page.
    pub next_cursor: Option<Uuid>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EntryListParams {
    /// Only entries with this status.
    pub status: Option<EntryStatus>,
    /// Cursor: return entries after this id.
    pub after: Option<Uuid>,
    /// Page size, 1 to 500. Default 50.
    pub limit: Option<i64>,
}

fn check_lines(lines: &[NewLine]) -> Result<()> {
    for (i, line) in lines.iter().enumerate() {
        check_amount(&format!("lines[{i}].amount"), line.amount.amount)?;
    }
    Ok(())
}

fn document_ref(value: Option<&str>) -> Result<Option<&str>> {
    value.map(|d| non_empty("document_ref", d)).transpose()
}

async fn insert_lines(tx: &mut TenantTx, entry_id: Uuid, lines: &[NewLine]) -> Result<()> {
    let taxed: Vec<TaxedLine> = lines
        .iter()
        .map(|l| TaxedLine {
            account_id: l.account_id,
            tax_code: l.tax_code.as_deref(),
        })
        .collect();
    let tax_codes = tax::resolve(tx, &taxed).await?;
    for (i, (line, tax_code)) in lines.iter().zip(tax_codes).enumerate() {
        sqlx::query!(
            "INSERT INTO journal_lines
               (id, tenant_id, entry_id, line_no, account_id, side, amount, currency, tax_code)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            new_id(),
            tx.tenant_id().0,
            entry_id,
            i as i32 + 1,
            line.account_id,
            line.side as Side,
            line.amount.amount,
            line.amount.currency.as_str(),
            tax_code,
        )
        .execute(tx.conn())
        .await?;
    }
    Ok(())
}

async fn lines_of(tx: &mut TenantTx, entry_ids: &[Uuid]) -> Result<HashMap<Uuid, Vec<Line>>> {
    let rows = sqlx::query_as!(
        LineRow,
        r#"SELECT id, entry_id, line_no, account_id, side AS "side: Side", amount, currency,
                  tax_code
         FROM journal_lines WHERE entry_id = ANY($1)
         ORDER BY entry_id, line_no"#,
        entry_ids,
    )
    .fetch_all(tx.conn())
    .await?;
    let mut lines: HashMap<Uuid, Vec<Line>> = HashMap::new();
    for row in rows {
        let entry_id = row.entry_id;
        lines.entry(entry_id).or_default().push(row.into_line()?);
    }
    Ok(lines)
}

async fn find(tx: &mut TenantTx, id: Uuid) -> Result<Option<Entry>> {
    let Some(row) = sqlx::query_as!(
        EntryRow,
        r#"SELECT e.id, e.status AS "status: EntryStatus", e.sequence_no, e.booking_date,
                  e.document_ref, e.description, e.reverses_id,
                  (SELECT r.id FROM journal_entries r WHERE r.reverses_id = e.id) AS "reversed_by_id?",
                  e.posted_at, e.created_at, e.updated_at
         FROM journal_entries e WHERE e.id = $1"#,
        id,
    )
    .fetch_optional(tx.conn())
    .await?
    else {
        return Ok(None);
    };
    let lines = lines_of(tx, &[id]).await?.remove(&id).unwrap_or_default();
    Ok(Some(row.with_lines(lines)))
}

/// Locks a draft entry for a change. Posted entries answer 409.
async fn lock_draft(tx: &mut TenantTx, id: Uuid) -> Result<()> {
    let status = sqlx::query_scalar!(
        r#"SELECT status AS "status: EntryStatus" FROM journal_entries WHERE id = $1 FOR UPDATE"#,
        id,
    )
    .fetch_optional(tx.conn())
    .await?
    .ok_or(Error::NotFound)?;
    if status == EntryStatus::Posted {
        return Err(Error::Conflict(
            "posted entries cannot be changed; reverse them instead".into(),
        ));
    }
    Ok(())
}

/// Posts the locked draft `id`: checks it, takes the tenant's next sequence
/// number and publishes `accounting.entry.posted`.
async fn post_draft(tx: &mut TenantTx, id: Uuid) -> Result<Entry> {
    let entry = find(tx, id).await?.ok_or(Error::NotFound)?;
    if entry.booking_date.is_none() {
        return Err(Error::Validation(
            "booking_date is required for posting".into(),
        ));
    }
    if entry.document_ref.is_none() {
        return Err(Error::Validation(
            "document_ref is required for posting".into(),
        ));
    }
    let Some(first) = entry.lines.first() else {
        return Err(Error::Validation(
            "an entry needs lines to be posted".into(),
        ));
    };
    let currency = first.amount.currency;
    if entry.lines.iter().any(|l| l.amount.currency != currency) {
        return Err(Error::Validation(
            "all lines must have the same currency".into(),
        ));
    }
    let sum = |side: Side| -> Decimal {
        entry
            .lines
            .iter()
            .filter(|l| l.side == side)
            .map(|l| l.amount.amount)
            .sum()
    };
    let (debit, credit) = (sum(Side::Debit), sum(Side::Credit));
    if debit != credit || debit <= Decimal::ZERO {
        return Err(Error::Validation(format!(
            "debit ({debit}) and credit ({credit}) must be equal and positive"
        )));
    }
    // The accounts may have changed since the lines were drafted.
    let taxed: Vec<TaxedLine> = entry
        .lines
        .iter()
        .map(|l| TaxedLine {
            account_id: l.account_id,
            tax_code: l.tax_code.as_deref(),
        })
        .collect();
    let resolved = tax::resolve(tx, &taxed).await?;
    if entry
        .lines
        .iter()
        .zip(&resolved)
        .any(|(l, code)| &l.tax_code != code)
    {
        return Err(Error::Validation(
            "tax codes of the lines no longer match their automatic accounts".into(),
        ));
    }

    // The row lock on the tenant's counter serializes concurrent postings; a
    // rolled-back posting releases its number again, so there are no gaps.
    let sequence_no = sqlx::query_scalar!(
        "INSERT INTO journal_sequences (tenant_id, last_no) VALUES ($1, 1)
         ON CONFLICT (tenant_id) DO UPDATE SET last_no = journal_sequences.last_no + 1
         RETURNING last_no",
        tx.tenant_id().0,
    )
    .fetch_one(tx.conn())
    .await?;
    sqlx::query!(
        "UPDATE journal_entries
         SET status = 'posted', sequence_no = $2, posted_at = now(), updated_at = now()
         WHERE id = $1",
        id,
        sequence_no,
    )
    .execute(tx.conn())
    .await?;

    let entry = find(tx, id).await?.ok_or(Error::NotFound)?;
    tx.publish("accounting.entry.posted", &entry).await?;
    Ok(entry)
}

/// List journal entries with their lines, ordered by id (creation time),
/// cursor-paginated.
#[utoipa::path(
    get,
    path = "/v1/accounting/entries",
    tag = "accounting",
    params(EntryListParams),
    responses((status = 200, body = EntryPage))
)]
pub async fn list(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Query(params): Query<EntryListParams>,
) -> Result<Json<EntryPage>> {
    let limit = page_limit(params.limit)?;
    let mut tx = ctx.begin(&state.db).await?;
    // One extra row tells whether another page exists.
    let mut rows = sqlx::query_as!(
        EntryRow,
        r#"SELECT e.id, e.status AS "status: EntryStatus", e.sequence_no, e.booking_date,
                  e.document_ref, e.description, e.reverses_id,
                  (SELECT r.id FROM journal_entries r WHERE r.reverses_id = e.id) AS "reversed_by_id?",
                  e.posted_at, e.created_at, e.updated_at
         FROM journal_entries e
         WHERE ($1::uuid IS NULL OR e.id > $1)
           AND ($2::text IS NULL OR e.status = $2)
         ORDER BY e.id
         LIMIT $3"#,
        params.after,
        params.status as Option<EntryStatus>,
        limit + 1,
    )
    .fetch_all(tx.conn())
    .await?;
    let next_cursor = paginate(&mut rows, limit, |e| e.id);
    let ids: Vec<Uuid> = rows.iter().map(|e| e.id).collect();
    let mut lines = lines_of(&mut tx, &ids).await?;
    tx.commit().await?;

    let items = rows
        .into_iter()
        .map(|row| {
            let entry_lines = lines.remove(&row.id).unwrap_or_default();
            row.with_lines(entry_lines)
        })
        .collect();
    Ok(Json(EntryPage { items, next_cursor }))
}

/// Create a draft entry.
#[utoipa::path(
    post,
    path = "/v1/accounting/entries",
    tag = "accounting",
    params(DryRun),
    request_body = NewEntry,
    responses(
        (status = 201, body = Entry),
        (status = 422, description = "Invalid input, e.g. an unknown account")
    )
)]
pub async fn create(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    dry_run: DryRun,
    Json(new): Json<NewEntry>,
) -> Result<Response> {
    let document_ref = document_ref(new.document_ref.as_deref())?;
    let description = new
        .description
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    check_lines(&new.lines)?;

    let mut tx = ctx.begin(&state.db).await?;
    let id = new_id();
    sqlx::query!(
        "INSERT INTO journal_entries (id, tenant_id, booking_date, document_ref, description)
         VALUES ($1, $2, $3, $4, $5)",
        id,
        tx.tenant_id().0,
        new.booking_date,
        document_ref,
        description,
    )
    .execute(tx.conn())
    .await?;
    insert_lines(&mut tx, id, &new.lines).await?;
    let entry = find(&mut tx, id).await?.ok_or(Error::NotFound)?;
    tx.publish("accounting.entry.created", &entry).await?;
    dry_run.respond(tx, StatusCode::CREATED, entry).await
}

/// Get one entry with its lines.
#[utoipa::path(
    get,
    path = "/v1/accounting/entries/{id}",
    tag = "accounting",
    params(("id" = Uuid, Path, description = "Entry id")),
    responses((status = 200, body = Entry), (status = 404, description = "Not found"))
)]
pub async fn get(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(id): Path<Uuid>,
) -> Result<Json<Entry>> {
    let mut tx = ctx.begin(&state.db).await?;
    let entry = find(&mut tx, id).await?;
    tx.commit().await?;
    entry.map(Json).ok_or(Error::NotFound)
}

/// Change a draft entry. Fields left out stay as they are; `lines` replaces
/// all lines.
#[utoipa::path(
    patch,
    path = "/v1/accounting/entries/{id}",
    tag = "accounting",
    params(("id" = Uuid, Path, description = "Entry id"), DryRun),
    request_body = EntryChanges,
    responses(
        (status = 200, body = Entry),
        (status = 404, description = "Not found"),
        (status = 409, description = "The entry is posted"),
        (status = 422, description = "Invalid input")
    )
)]
pub async fn update(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(id): Path<Uuid>,
    dry_run: DryRun,
    Json(changes): Json<EntryChanges>,
) -> Result<Response> {
    let document_ref = document_ref(changes.document_ref.as_deref())?;
    let description = changes.description.as_deref().map(str::trim);
    if let Some(lines) = &changes.lines {
        check_lines(lines)?;
    }

    let mut tx = ctx.begin(&state.db).await?;
    lock_draft(&mut tx, id).await?;
    sqlx::query!(
        "UPDATE journal_entries SET
           booking_date = coalesce($2, booking_date),
           document_ref = coalesce($3, document_ref),
           description = coalesce($4, description),
           updated_at = now()
         WHERE id = $1",
        id,
        changes.booking_date,
        document_ref,
        description,
    )
    .execute(tx.conn())
    .await?;
    if let Some(lines) = &changes.lines {
        sqlx::query!("DELETE FROM journal_lines WHERE entry_id = $1", id)
            .execute(tx.conn())
            .await?;
        insert_lines(&mut tx, id, lines).await?;
    }
    let entry = find(&mut tx, id).await?.ok_or(Error::NotFound)?;
    tx.publish("accounting.entry.updated", &entry).await?;
    dry_run.respond(tx, StatusCode::OK, entry).await
}

/// Delete a draft entry. Returns the deleted entry.
#[utoipa::path(
    delete,
    path = "/v1/accounting/entries/{id}",
    tag = "accounting",
    params(("id" = Uuid, Path, description = "Entry id"), DryRun),
    responses(
        (status = 200, body = Entry, description = "The deleted entry"),
        (status = 404, description = "Not found"),
        (status = 409, description = "The entry is posted")
    )
)]
pub async fn delete(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(id): Path<Uuid>,
    dry_run: DryRun,
) -> Result<Response> {
    let mut tx = ctx.begin(&state.db).await?;
    lock_draft(&mut tx, id).await?;
    let entry = find(&mut tx, id).await?.ok_or(Error::NotFound)?;
    sqlx::query!("DELETE FROM journal_lines WHERE entry_id = $1", id)
        .execute(tx.conn())
        .await?;
    sqlx::query!("DELETE FROM journal_entries WHERE id = $1", id)
        .execute(tx.conn())
        .await?;
    tx.publish("accounting.entry.deleted", &entry).await?;
    dry_run.respond(tx, StatusCode::OK, entry).await
}

/// Post a draft entry: it must have a booking date, a document reference and
/// lines in one currency whose debits equal their credits, and each line's
/// tax code must still fit its account. It gets the
/// tenant's next sequence number and can no longer be changed.
#[utoipa::path(
    post,
    path = "/v1/accounting/entries/{id}/post",
    tag = "accounting",
    params(("id" = Uuid, Path, description = "Entry id"), DryRun),
    responses(
        (status = 200, body = Entry),
        (status = 404, description = "Not found"),
        (status = 409, description = "The entry is already posted"),
        (status = 422, description = "The entry cannot be posted, e.g. it is not balanced")
    )
)]
pub async fn post(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(id): Path<Uuid>,
    dry_run: DryRun,
) -> Result<Response> {
    let mut tx = ctx.begin(&state.db).await?;
    lock_draft(&mut tx, id).await?;
    let entry = post_draft(&mut tx, id).await?;
    dry_run.respond(tx, StatusCode::OK, entry).await
}

/// Reverse a posted entry: creates and posts a mirror entry (same accounts and
/// amounts, debit and credit swapped) that references the original. An entry
/// can be reversed once.
#[utoipa::path(
    post,
    path = "/v1/accounting/entries/{id}/reverse",
    tag = "accounting",
    params(("id" = Uuid, Path, description = "Id of the entry to reverse"), DryRun),
    request_body(content = Option<Reversal>, description = "Optional"),
    responses(
        (status = 201, body = Entry, description = "The posted reversal entry"),
        (status = 404, description = "Not found"),
        (status = 409, description = "The entry is a draft or already reversed"),
        (status = 422, description = "Invalid input")
    )
)]
pub async fn reverse(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(id): Path<Uuid>,
    dry_run: DryRun,
    body: Option<Json<Reversal>>,
) -> Result<Response> {
    let reversal = body.map(|Json(r)| r).unwrap_or_default();
    let document_ref = document_ref(reversal.document_ref.as_deref())?;

    let mut tx = ctx.begin(&state.db).await?;
    // Lock the original, so that concurrent reversals of it run one after the other.
    sqlx::query!(
        "SELECT id FROM journal_entries WHERE id = $1 FOR UPDATE",
        id
    )
    .fetch_optional(tx.conn())
    .await?
    .ok_or(Error::NotFound)?;
    let original = find(&mut tx, id).await?.ok_or(Error::NotFound)?;
    if original.status != EntryStatus::Posted {
        return Err(Error::Conflict(
            "only posted entries can be reversed; change or delete the draft instead".into(),
        ));
    }
    if original.reversed_by_id.is_some() {
        return Err(Error::Conflict("entry is already reversed".into()));
    }

    let description = match reversal.description.as_deref().map(str::trim) {
        Some(description) => description.to_string(),
        None => {
            let no = original.sequence_no.unwrap_or_default();
            match original.description.as_str() {
                "" => format!("Storno of #{no}"),
                text => format!("Storno of #{no}: {text}"),
            }
        }
    };
    let reversal_id = new_id();
    sqlx::query!(
        "INSERT INTO journal_entries
           (id, tenant_id, booking_date, document_ref, description, reverses_id)
         VALUES ($1, $2, coalesce($3, current_date), $4, $5, $6)",
        reversal_id,
        tx.tenant_id().0,
        reversal.booking_date,
        document_ref.or(original.document_ref.as_deref()),
        description,
        id,
    )
    .execute(tx.conn())
    .await
    .map_err(|err| {
        if violated_constraint(&err) == Some(REVERSES_CONSTRAINT) {
            return Error::Conflict("entry is already reversed".into());
        }
        err.into()
    })?;
    let mirror: Vec<NewLine> = original
        .lines
        .iter()
        .map(|l| NewLine {
            account_id: l.account_id,
            side: l.side.opposite(),
            amount: l.amount,
            tax_code: l.tax_code.clone(),
        })
        .collect();
    insert_lines(&mut tx, reversal_id, &mirror).await?;
    let entry = post_draft(&mut tx, reversal_id).await?;
    dry_run.respond(tx, StatusCode::CREATED, entry).await
}
