//! Helpers shared by accounts and journal entries.

use core_kernel::{Currency, Error, Result};
use rust_decimal::Decimal;
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

/// Amounts are stored as numeric(19, 4).
const AMOUNT_SCALE: u32 = 4;
const AMOUNT_INTEGER_DIGITS: u32 = 15;

/// OpenAPI shape of [`core_kernel::Money`].
#[derive(utoipa::ToSchema)]
#[schema(as = Money)]
#[allow(dead_code)]
pub(crate) struct MoneySchema {
    /// Decimal amount as a string, e.g. `"19.99"`.
    #[schema(example = "19.99")]
    amount: String,
    /// ISO 4217 currency code.
    #[schema(example = "EUR", pattern = "^[A-Z]{3}$")]
    currency: String,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PageParams {
    /// Cursor: return items after this id.
    pub after: Option<Uuid>,
    /// Page size, 1 to 500. Default 50.
    pub limit: Option<i64>,
}

impl PageParams {
    pub(crate) fn limit(&self) -> Result<i64> {
        page_limit(self.limit)
    }
}

/// The requested page size, or the default.
pub(crate) fn page_limit(limit: Option<i64>) -> Result<i64> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(Error::Validation(format!("limit must be 1 to {MAX_LIMIT}")));
    }
    Ok(limit)
}

/// Cuts `items` (fetched with `limit + 1`) to one page and returns the cursor
/// of the next page, if there is one.
pub(crate) fn paginate<T>(items: &mut Vec<T>, limit: i64, id: impl Fn(&T) -> Uuid) -> Option<Uuid> {
    if items.len() as i64 > limit {
        items.truncate(limit as usize);
        items.last().map(id)
    } else {
        None
    }
}

pub(crate) fn non_empty<'a>(field: &str, value: &'a str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        return Err(Error::Validation(format!("{field} must not be empty")));
    }
    Ok(value)
}

/// A line amount: positive, at most 4 decimal places, fits numeric(19, 4).
pub(crate) fn check_amount(field: &str, amount: Decimal) -> Result<()> {
    if amount <= Decimal::ZERO {
        return Err(Error::Validation(format!("{field} must be positive")));
    }
    if amount.normalize().scale() > AMOUNT_SCALE {
        return Err(Error::Validation(format!(
            "{field} has more than {AMOUNT_SCALE} decimal places"
        )));
    }
    if amount.trunc() >= Decimal::from(10_i64.pow(AMOUNT_INTEGER_DIGITS)) {
        return Err(Error::Validation(format!("{field} is too large")));
    }
    Ok(())
}

/// Reads a `char(3)` currency column.
pub(crate) fn currency_column(column: &str, value: &str) -> sqlx::Result<Currency> {
    value.parse().map_err(|err| sqlx::Error::ColumnDecode {
        index: column.into(),
        source: Box::new(err),
    })
}

/// Name of the database constraint `err` violated, if any.
pub(crate) fn violated_constraint(err: &sqlx::Error) -> Option<&str> {
    err.as_database_error().and_then(|db| db.constraint())
}
