//! Standard charts of accounts (SKR03, SKR04) a tenant picks once at setup.
//!
//! The templates are the data files in `charts/`, embedded at compile time.

use std::sync::LazyLock;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use chrono::{DateTime, Utc};
use core_host::{CoreState, DryRun};
use core_kernel::{Error, Result, TenantCtx, new_id};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::accounts::AccountType;
use crate::common::violated_constraint;
use crate::tax;

/// One chart per tenant, see the migration.
const TENANT_CONSTRAINT: &str = "accounting_charts_tenant_id_key";
/// Unique constraint on account numbers.
const NUMBER_CONSTRAINT: &str = "accounts_tenant_id_number_key";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Chart {
    /// DATEV SKR 03 (Prozessgliederungsprinzip).
    Skr03,
    /// DATEV SKR 04 (Abschlussgliederungsprinzip).
    Skr04,
}

#[derive(Debug)]
pub(crate) struct TemplateAccount {
    pub number: &'static str,
    pub account_type: AccountType,
    /// Set on an automatic account.
    pub tax_code: Option<&'static str>,
    pub name: &'static str,
}

#[derive(Debug)]
pub(crate) struct Template {
    pub version: &'static str,
    pub accounts: Vec<TemplateAccount>,
}

const HEADER: &str = "number;type;automatic;tax_code;name";
const VERSION_PREFIX: &str = "# Template version: ";

static SKR03: LazyLock<Template> =
    LazyLock::new(|| parse(include_str!("../charts/skr03.csv")).expect("charts/skr03.csv"));
static SKR04: LazyLock<Template> =
    LazyLock::new(|| parse(include_str!("../charts/skr04.csv")).expect("charts/skr04.csv"));

impl Chart {
    pub(crate) fn template(self) -> &'static Template {
        match self {
            Chart::Skr03 => &SKR03,
            Chart::Skr04 => &SKR04,
        }
    }
}

/// Parses a chart data file; see the comment at the top of each file.
fn parse(src: &'static str) -> std::result::Result<Template, String> {
    let mut version = None;
    let mut header = false;
    let mut accounts: Vec<TemplateAccount> = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let at = |msg: &str| format!("line {}: {msg}", i + 1);
        if let Some(v) = line.strip_prefix(VERSION_PREFIX) {
            version = Some(v.trim());
        }
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if !header {
            if line != HEADER {
                return Err(at(&format!("expected header `{HEADER}`")));
            }
            header = true;
            continue;
        }
        let fields: Vec<&str> = line.split(';').collect();
        let [number, account_type, automatic, tax_code, name] = fields[..] else {
            return Err(at("expected 5 fields"));
        };
        if number.len() != 4 || !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(at("account number must have 4 digits"));
        }
        let account_type = match account_type {
            "asset" => AccountType::Asset,
            "liability" => AccountType::Liability,
            "equity" => AccountType::Equity,
            "revenue" => AccountType::Revenue,
            "expense" => AccountType::Expense,
            other => return Err(at(&format!("unknown type `{other}`"))),
        };
        let tax_code = match (automatic, tax_code) {
            ("no", "") => None,
            ("yes", code) if !code.is_empty() => Some(code),
            _ => return Err(at("an automatic account needs a tax code, others none")),
        };
        if name.trim().is_empty() || name.trim() != name {
            return Err(at("name must not be empty or padded"));
        }
        if accounts.iter().any(|a| a.number == number) {
            return Err(at(&format!("duplicate account number {number}")));
        }
        accounts.push(TemplateAccount {
            number,
            account_type,
            tax_code,
            name,
        });
    }
    let version = version.ok_or("missing `# Template version:` line")?;
    if accounts.is_empty() {
        return Err("no accounts".into());
    }
    Ok(Template { version, accounts })
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ChartChoice {
    pub chart: Chart,
}

/// The standard chart a tenant was set up with.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChartInfo {
    pub chart: Chart,
    /// Version of the chart template that was applied, e.g. `skr03-2025.1`.
    pub version: String,
    pub set_up_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChartSetup {
    #[serde(flatten)]
    pub chart: ChartInfo,
    /// Number of accounts created.
    pub accounts_created: usize,
}

/// The standard chart the tenant was set up with. 404 if it was not set up.
#[utoipa::path(
    get,
    path = "/v1/accounting/chart",
    tag = "accounting",
    responses((status = 200, body = ChartInfo), (status = 404, description = "No chart set up"))
)]
pub async fn get(State(state): State<CoreState>, ctx: TenantCtx) -> Result<Json<ChartInfo>> {
    let mut tx = ctx.begin(&state.db).await?;
    let info = sqlx::query_as!(
        ChartInfo,
        r#"SELECT chart AS "chart: Chart", version, set_up_at FROM accounting_charts"#,
    )
    .fetch_optional(tx.conn())
    .await?;
    tx.commit().await?;
    info.map(Json).ok_or(Error::NotFound)
}

/// Set up the tenant's chart of accounts from a standard chart (SKR03 or
/// SKR04). Creates the chart's accounts, including its automatic accounts
/// with their tax codes. Works once per tenant; switching charts is not
/// supported.
#[utoipa::path(
    post,
    path = "/v1/accounting/chart/setup",
    tag = "accounting",
    params(DryRun),
    request_body = ChartChoice,
    responses(
        (status = 201, body = ChartSetup),
        (status = 409, description = "A chart is already set up, or an account number of the chart already exists"),
        (status = 422, description = "Invalid input")
    )
)]
pub async fn setup(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    dry_run: DryRun,
    Json(choice): Json<ChartChoice>,
) -> Result<Response> {
    let template = choice.chart.template();

    let mut tx = ctx.begin(&state.db).await?;
    let info = sqlx::query_as!(
        ChartInfo,
        r#"INSERT INTO accounting_charts (id, tenant_id, chart, version)
         VALUES ($1, $2, $3, $4)
         RETURNING chart AS "chart: Chart", version, set_up_at"#,
        new_id(),
        tx.tenant_id().0,
        choice.chart as Chart,
        template.version,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(|err| {
        if violated_constraint(&err) == Some(TENANT_CONSTRAINT) {
            return Error::Conflict("the chart of accounts is already set up".into());
        }
        err.into()
    })?;

    for account in &template.accounts {
        if let Some(code) = account.tax_code {
            let what = format!("account {}", account.number);
            tax::check_account_code(&mut tx, code, account.account_type, &what).await?;
        }
    }
    let ids: Vec<Uuid> = template.accounts.iter().map(|_| new_id()).collect();
    let numbers: Vec<&str> = template.accounts.iter().map(|a| a.number).collect();
    let names: Vec<&str> = template.accounts.iter().map(|a| a.name).collect();
    let types: Vec<&str> = template
        .accounts
        .iter()
        .map(|a| a.account_type.as_str())
        .collect();
    let tax_codes: Vec<Option<&str>> = template.accounts.iter().map(|a| a.tax_code).collect();
    sqlx::query!(
        "INSERT INTO accounts (id, tenant_id, number, name, type, tax_code)
         SELECT t.id, $1, t.number, t.name, t.type, t.tax_code
         FROM UNNEST($2::uuid[], $3::text[], $4::text[], $5::text[], $6::text[])
           AS t(id, number, name, type, tax_code)",
        tx.tenant_id().0,
        &ids,
        &numbers as &[&str],
        &names as &[&str],
        &types as &[&str],
        &tax_codes as &[Option<&str>],
    )
    .execute(tx.conn())
    .await
    .map_err(|err| {
        if violated_constraint(&err) == Some(NUMBER_CONSTRAINT) {
            return Error::Conflict(
                "an account number of the chart already exists; set up the chart before adding accounts".into(),
            );
        }
        err.into()
    })?;

    let result = ChartSetup {
        chart: info,
        accounts_created: template.accounts.len(),
    };
    tx.publish("accounting.chart.set_up", &result).await?;
    dry_run.respond(tx, StatusCode::CREATED, result).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_parse() {
        for chart in [Chart::Skr03, Chart::Skr04] {
            let template = chart.template();
            assert!(template.accounts.len() > 50, "{chart:?}");
            assert!(template.accounts.iter().any(|a| a.tax_code.is_some()));
        }
        assert_eq!(Chart::Skr03.template().version, "skr03-2025.1");
        assert_eq!(Chart::Skr04.template().version, "skr04-2025.1");
    }

    #[test]
    fn automatic_accounts_fit_their_type() {
        for chart in [Chart::Skr03, Chart::Skr04] {
            for a in &chart.template().accounts {
                if a.tax_code.is_some() {
                    assert!(
                        matches!(a.account_type, AccountType::Revenue | AccountType::Expense),
                        "{chart:?} {}",
                        a.number
                    );
                }
            }
        }
    }

    #[test]
    fn rejects_bad_rows() {
        let base = "# Template version: x\nnumber;type;automatic;tax_code;name\n";
        for row in [
            "120;asset;no;;Bank",
            "1200;income;no;;Bank",
            "1200;asset;yes;;Bank",
            "1200;asset;no;VAT19_IN;Bank",
            "1200;asset;no;;",
            "1200;asset;no;Bank",
        ] {
            let src: &'static str = Box::leak(format!("{base}{row}\n").into_boxed_str());
            assert!(parse(src).is_err(), "{row}");
        }
        assert!(parse("number;type;automatic;tax_code;name\n1200;asset;no;;Bank\n").is_err());
        assert!(parse("# Template version: x\n1200;asset;no;;Bank\n").is_err());
        let dup = "# Template version: x\nnumber;type;automatic;tax_code;name\n1200;asset;no;;A\n1200;asset;no;;B\n";
        assert!(parse(dup).is_err());
    }
}
