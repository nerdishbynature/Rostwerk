//! Rostwerk tax codes and the rules for which account a code may be used on.
//!
//! Tax codes are reference data shared by all tenants (`accounting_tax_codes`,
//! filled by migrations). They are independent of DATEV; the DATEV export maps
//! them to BU-Schlüssel.

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use core_host::CoreState;
use core_kernel::{Error, Result, TenantCtx, TenantTx};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::accounts::AccountType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum TaxDirection {
    /// Sales (Umsatzsteuer). Allowed on revenue accounts.
    Output,
    /// Purchases (Vorsteuer). Allowed on expense and asset accounts.
    Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum TaxTreatment {
    Standard,
    /// Steuerschuldnerschaft des Leistungsempfängers (§ 13b UStG).
    ReverseCharge,
    /// Intra-EU supply or acquisition.
    IntraEu,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TaxCode {
    #[schema(example = "VAT19_OUT")]
    pub code: String,
    pub name: String,
    /// Rate in percent, as a decimal string.
    #[schema(value_type = String, example = "19")]
    pub rate: Decimal,
    pub direction: TaxDirection,
    pub treatment: TaxTreatment,
}

/// Whether a tax code with `direction` may be used on an account of `account_type`.
/// Balance accounts other than assets (purchased fixed assets carry input VAT)
/// never carry a tax code.
pub(crate) fn fits(direction: TaxDirection, account_type: AccountType) -> bool {
    match direction {
        TaxDirection::Output => account_type == AccountType::Revenue,
        TaxDirection::Input => matches!(account_type, AccountType::Expense | AccountType::Asset),
    }
}

/// Directions of the given codes; unknown codes are missing from the map.
async fn directions(tx: &mut TenantTx, codes: &[String]) -> Result<HashMap<String, TaxDirection>> {
    let rows = sqlx::query!(
        r#"SELECT code, direction AS "direction: TaxDirection"
         FROM accounting_tax_codes WHERE code = ANY($1)"#,
        codes,
    )
    .fetch_all(tx.conn())
    .await?;
    Ok(rows.into_iter().map(|r| (r.code, r.direction)).collect())
}

/// Checks that `code` exists and may be used on an account of `account_type`.
/// `what` names the account in the error, e.g. `account 1200`.
pub(crate) async fn check_account_code(
    tx: &mut TenantTx,
    code: &str,
    account_type: AccountType,
    what: &str,
) -> Result<()> {
    let known = directions(tx, &[code.to_string()]).await?;
    let direction = known
        .get(code)
        .ok_or_else(|| Error::Validation(format!("unknown tax code: {code}")))?;
    if !fits(*direction, account_type) {
        return Err(Error::Validation(misfit(
            code,
            *direction,
            account_type,
            what,
        )));
    }
    Ok(())
}

fn misfit(code: &str, direction: TaxDirection, account_type: AccountType, what: &str) -> String {
    let direction = match direction {
        TaxDirection::Output => "output",
        TaxDirection::Input => "input",
    };
    format!(
        "{direction} tax code {code} cannot be used on {what} ({})",
        account_type.as_str()
    )
}

/// A journal line as far as tax is concerned.
pub(crate) struct TaxedLine<'a> {
    pub account_id: Uuid,
    pub tax_code: Option<&'a str>,
}

/// Resolves and checks the tax code of each line, in order:
///
/// - every account must exist;
/// - on an automatic account the line takes the account's code, and a
///   different code is rejected;
/// - a code must exist and fit the account's type (see [`fits`]).
///
/// Returns the effective tax code per line.
pub(crate) async fn resolve(
    tx: &mut TenantTx,
    lines: &[TaxedLine<'_>],
) -> Result<Vec<Option<String>>> {
    let ids: Vec<Uuid> = lines.iter().map(|l| l.account_id).collect();
    let accounts: HashMap<Uuid, (String, AccountType, Option<String>)> = sqlx::query!(
        r#"SELECT id, number, type AS "account_type: AccountType", tax_code
         FROM accounts WHERE id = ANY($1)"#,
        &ids,
    )
    .fetch_all(tx.conn())
    .await?
    .into_iter()
    .map(|r| (r.id, (r.number, r.account_type, r.tax_code)))
    .collect();

    let mut missing: Vec<String> = ids
        .iter()
        .filter(|id| !accounts.contains_key(id))
        .map(Uuid::to_string)
        .collect();
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        return Err(Error::Validation(format!(
            "unknown account: {}",
            missing.join(", ")
        )));
    }

    let mut effective = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let (number, _, automatic) = &accounts[&line.account_id];
        let code = match (automatic, line.tax_code) {
            (Some(auto), Some(given)) if auto != given => {
                return Err(Error::Validation(format!(
                    "lines[{i}]: account {number} is an automatic account with tax code {auto}; \
                     tax code {given} is not allowed on it"
                )));
            }
            (Some(auto), _) => Some(auto.clone()),
            (None, given) => given.map(str::to_string),
        };
        effective.push(code);
    }

    let mut codes: Vec<String> = effective.iter().flatten().cloned().collect();
    codes.sort();
    codes.dedup();
    let known = directions(tx, &codes).await?;
    for (i, (line, code)) in lines.iter().zip(&effective).enumerate() {
        let Some(code) = code else { continue };
        let (number, account_type, _) = &accounts[&line.account_id];
        let Some(direction) = known.get(code) else {
            return Err(Error::Validation(format!(
                "lines[{i}].tax_code: unknown tax code: {code}"
            )));
        };
        if !fits(*direction, *account_type) {
            let message = misfit(
                code,
                *direction,
                *account_type,
                &format!("account {number}"),
            );
            return Err(Error::Validation(format!("lines[{i}]: {message}")));
        }
    }
    Ok(effective)
}

/// List the tax codes lines and automatic accounts can use, ordered by code.
#[utoipa::path(
    get,
    path = "/v1/accounting/tax-codes",
    tag = "accounting",
    responses((status = 200, body = [TaxCode]))
)]
pub async fn list(State(state): State<CoreState>, ctx: TenantCtx) -> Result<Json<Vec<TaxCode>>> {
    let mut tx = ctx.begin(&state.db).await?;
    let mut codes = sqlx::query_as!(
        TaxCode,
        r#"SELECT code, name, rate, direction AS "direction: TaxDirection",
                  treatment AS "treatment: TaxTreatment"
         FROM accounting_tax_codes ORDER BY code"#,
    )
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;
    for code in &mut codes {
        code.rate = code.rate.normalize();
    }
    Ok(Json(codes))
}
