//! Accounting module: SKR03/SKR04 chart setup and tax codes on journal lines.

mod common;

use std::collections::HashMap;

use axum::Router;
use axum::http::StatusCode;
use common::{call, call_with, create_tenant, harness, token};
use serde_json::{Value, json};

/// Number of accounts in a chart data file.
fn template_size(src: &str) -> usize {
    src.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .count()
        - 1 // header
}

const SKR03: &str = include_str!("../../../crates/module-accounting/charts/skr03.csv");
const SKR04: &str = include_str!("../../../crates/module-accounting/charts/skr04.csv");

async fn setup_chart(app: &Router, tenant: &str, chart: &str) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        "/v1/accounting/chart/setup",
        &token(tenant),
        Some(json!({ "chart": chart })),
    )
    .await
}

/// All accounts of the tenant by number.
async fn accounts(app: &Router, tenant: &str) -> HashMap<String, Value> {
    let (status, page) = call(
        app,
        "GET",
        "/v1/accounting/accounts?limit=500",
        &token(tenant),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page["next_cursor"].is_null());
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["number"].as_str().unwrap().to_string(), a.clone()))
        .collect()
}

fn id(accounts: &HashMap<String, Value>, number: &str) -> String {
    accounts[number]["id"].as_str().unwrap().to_string()
}

fn line(account: &str, side: &str, amount: &str, tax_code: Option<&str>) -> Value {
    let mut line = json!({
        "account_id": account,
        "side": side,
        "amount": { "amount": amount, "currency": "EUR" },
    });
    if let Some(code) = tax_code {
        line["tax_code"] = json!(code);
    }
    line
}

fn entry(lines: Vec<Value>) -> Value {
    json!({
        "booking_date": "2026-11-20",
        "document_ref": "ER-2026-042",
        "description": "Büromaterial",
        "lines": lines,
    })
}

async fn create_entry(app: &Router, tenant: &str, body: Value) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        "/v1/accounting/entries",
        &token(tenant),
        Some(body),
    )
    .await
}

async fn post(app: &Router, tenant: &str, entry: &Value) -> (StatusCode, Value) {
    let id = entry["id"].as_str().unwrap();
    call(
        app,
        "POST",
        &format!("/v1/accounting/entries/{id}/post"),
        &token(tenant),
        None,
    )
    .await
}

fn tax_codes(entry: &Value) -> Vec<Value> {
    entry["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["tax_code"].clone())
        .collect()
}

#[tokio::test]
async fn setup_skr03_once() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "SKR03 GmbH", &["accounting"]).await;

    let (status, body) = call(&h.app, "GET", "/v1/accounting/chart", &token(&t), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, body) = setup_chart(&h.app, &t, "skr03").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["chart"], "skr03");
    assert_eq!(body["version"], "skr03-2025.1");
    assert_eq!(body["accounts_created"], template_size(SKR03));

    let (status, chart) = call(&h.app, "GET", "/v1/accounting/chart", &token(&t), None).await;
    assert_eq!(status, StatusCode::OK, "{chart}");
    assert_eq!(chart["chart"], "skr03");
    assert_eq!(chart["set_up_at"], body["set_up_at"]);

    let accounts = accounts(&h.app, &t).await;
    assert_eq!(accounts.len(), template_size(SKR03));
    assert_eq!(accounts["1200"]["name"], "Bank");
    assert_eq!(accounts["1200"]["type"], "asset");
    assert!(accounts["1200"]["tax_code"].is_null());
    assert_eq!(accounts["8400"]["name"], "Erlöse 19 % USt");
    assert_eq!(accounts["8400"]["type"], "revenue");
    assert_eq!(accounts["8400"]["tax_code"], "VAT19_OUT");
    assert_eq!(accounts["3400"]["tax_code"], "VAT19_IN");
    assert_eq!(accounts["8125"]["tax_code"], "IGL_OUT");
    assert_eq!(accounts["1776"]["type"], "liability");

    // Once only, with the same or the other chart.
    for chart in ["skr03", "skr04"] {
        let (status, body) = setup_chart(&h.app, &t, chart).await;
        assert_eq!(status, StatusCode::CONFLICT, "{chart}: {body}");
    }
    assert_eq!(account_count(&h, &t).await, template_size(SKR03) as i64);

    let (status, body) = setup_chart(&h.app, &t, "skr05").await;
    assert!(status.is_client_error(), "{status} {body}");
}

/// Accounts of the tenant, counted with the owner role.
async fn account_count(h: &common::Harness, tenant: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM accounts WHERE tenant_id = $1::uuid")
        .bind(tenant)
        .fetch_one(&h.admin_pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn setup_skr04_once() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "SKR04 GmbH", &["accounting"]).await;

    let (status, body) = setup_chart(&h.app, &t, "skr04").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["chart"], "skr04");
    assert_eq!(body["version"], "skr04-2025.1");
    assert_eq!(body["accounts_created"], template_size(SKR04));

    let accounts = accounts(&h.app, &t).await;
    assert_eq!(accounts.len(), template_size(SKR04));
    assert_eq!(accounts["1800"]["name"], "Bank");
    assert_eq!(accounts["4400"]["tax_code"], "VAT19_OUT");
    assert_eq!(accounts["5400"]["tax_code"], "VAT19_IN");
    assert_eq!(accounts["3806"]["type"], "liability");

    let (status, body) = setup_chart(&h.app, &t, "skr04").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Other tenants are not affected.
    let other = create_tenant(&h.app, "Andere GmbH", &["accounting"]).await;
    let (status, body) = call(&h.app, "GET", "/v1/accounting/chart", &token(&other), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = setup_chart(&h.app, &other, "skr03").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn setup_dry_run_and_number_clash() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Dry Chart GmbH", &["accounting"]).await;

    let (status, body) = call_with(
        &h.app,
        "POST",
        "/v1/accounting/chart/setup",
        &token(&t),
        &[("Prefer", "dry-run")],
        Some(json!({ "chart": "skr03" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["result"]["accounts_created"], template_size(SKR03));
    assert_eq!(body["events"][0]["topic"], "accounting.chart.set_up");
    assert_eq!(account_count(&h, &t).await, 0);

    // A hand-made account with a number of the chart blocks the setup, which
    // then leaves nothing behind.
    let (status, body) = call(
        &h.app,
        "POST",
        "/v1/accounting/accounts",
        &token(&t),
        Some(json!({ "number": "1200", "name": "Sparkasse", "type": "asset" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = setup_chart(&h.app, &t, "skr03").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(account_count(&h, &t).await, 1);
    let (status, _) = call(&h.app, "GET", "/v1/accounting/chart", &token(&t), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn tax_codes_list() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Tax Codes GmbH", &["accounting"]).await;

    let (status, body) = call(&h.app, "GET", "/v1/accounting/tax-codes", &token(&t), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let codes: HashMap<&str, &Value> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["code"].as_str().unwrap(), c))
        .collect();
    for (code, rate, direction, treatment) in [
        ("VAT19_OUT", "19", "output", "standard"),
        ("VAT7_OUT", "7", "output", "standard"),
        ("VAT0_OUT", "0", "output", "standard"),
        ("VAT19_IN", "19", "input", "standard"),
        ("VAT7_IN", "7", "input", "standard"),
        ("RC19_IN", "19", "input", "reverse_charge"),
        ("RC_OUT", "0", "output", "reverse_charge"),
        ("IGL_OUT", "0", "output", "intra_eu"),
        ("IGE19_IN", "19", "input", "intra_eu"),
    ] {
        let c = codes.get(code).unwrap_or_else(|| panic!("{code} missing"));
        assert_eq!(c["rate"], rate, "{code}");
        assert_eq!(c["direction"], direction, "{code}");
        assert_eq!(c["treatment"], treatment, "{code}");
    }

    // Reference data: the application role cannot change it.
    let err = sqlx::query("UPDATE accounting_tax_codes SET rate = 20 WHERE code = 'VAT19_OUT'")
        .execute(&h.app_pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("migrations only"), "{err}");
}

#[tokio::test]
async fn posting_with_tax_codes() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Tax Posting GmbH", &["accounting"]).await;
    let (status, body) = setup_chart(&h.app, &t, "skr03").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let a = accounts(&h.app, &t).await;
    let (office, input_vat, payables) = (id(&a, "4930"), id(&a, "1576"), id(&a, "1600"));

    // Office supplies with 19 % input VAT, split by hand.
    let (status, draft) = create_entry(
        &h.app,
        &t,
        entry(vec![
            line(&office, "debit", "100.00", Some("VAT19_IN")),
            line(&input_vat, "debit", "19.00", None),
            line(&payables, "credit", "119.00", None),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{draft}");
    let (status, posted) = post(&h.app, &t, &draft).await;
    assert_eq!(status, StatusCode::OK, "{posted}");
    assert_eq!(
        tax_codes(&posted),
        vec![json!("VAT19_IN"), Value::Null, Value::Null]
    );

    // An automatic account supplies its code.
    let (status, draft) = create_entry(
        &h.app,
        &t,
        entry(vec![
            line(&id(&a, "3400"), "debit", "119.00", None),
            line(&payables, "credit", "119.00", None),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{draft}");
    assert_eq!(tax_codes(&draft), vec![json!("VAT19_IN"), Value::Null]);
    let (status, posted) = post(&h.app, &t, &draft).await;
    assert_eq!(status, StatusCode::OK, "{posted}");

    // A reversal keeps the tax codes.
    let (status, reversal) = call(
        &h.app,
        "POST",
        &format!(
            "/v1/accounting/entries/{}/reverse",
            posted["id"].as_str().unwrap()
        ),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reversal}");
    assert_eq!(tax_codes(&reversal), vec![json!("VAT19_IN"), Value::Null]);

    // Rejected lines.
    for (account, code, expected) in [
        ("3400", "VAT7_IN", "automatic account"),
        ("4930", "VAT99_IN", "unknown tax code"),
        ("4930", "VAT19_OUT", "cannot be used on account 4930"),
        ("1600", "VAT19_IN", "cannot be used on account 1600"),
        ("0800", "VAT19_IN", "cannot be used on account 0800"),
        ("8200", "VAT19_IN", "cannot be used on account 8200"),
    ] {
        let (status, body) = create_entry(
            &h.app,
            &t,
            entry(vec![
                line(&id(&a, account), "debit", "10.00", Some(code)),
                line(&payables, "credit", "10.00", None),
            ]),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{account} {code}: {body}"
        );
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains(expected),
            "{account} {code}: {body}"
        );
    }

    // Output VAT on a revenue account, and on an asset purchase input VAT.
    let (status, draft) = create_entry(
        &h.app,
        &t,
        entry(vec![
            line(&id(&a, "1400"), "debit", "119.00", None),
            line(&id(&a, "8200"), "credit", "100.00", Some("VAT19_OUT")),
            line(&id(&a, "1776"), "credit", "19.00", None),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{draft}");
    let (status, draft) = create_entry(
        &h.app,
        &t,
        entry(vec![
            line(&id(&a, "0420"), "debit", "1000.00", Some("VAT19_IN")),
            line(&payables, "credit", "1000.00", None),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{draft}");
}

#[tokio::test]
async fn posting_rechecks_tax_codes() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Recheck GmbH", &["accounting"]).await;

    let mut ids = HashMap::new();
    for (number, account_type) in [("8000", "revenue"), ("1200", "asset")] {
        let (status, body) = call(
            &h.app,
            "POST",
            "/v1/accounting/accounts",
            &token(&t),
            Some(json!({ "number": number, "name": "Konto", "type": account_type })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        ids.insert(number, body["id"].as_str().unwrap().to_string());
    }
    let (status, draft) = create_entry(
        &h.app,
        &t,
        entry(vec![
            line(&ids["1200"], "debit", "107.00", None),
            line(&ids["8000"], "credit", "107.00", Some("VAT7_OUT")),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{draft}");

    // The account becomes a liability while the entry is a draft.
    let (status, body) = call(
        &h.app,
        "PATCH",
        &format!("/v1/accounting/accounts/{}", ids["8000"]),
        &token(&t),
        Some(json!({ "type": "liability" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = post(&h.app, &t, &draft).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("VAT7_OUT"),
        "{body}"
    );
}

#[tokio::test]
async fn automatic_accounts_by_hand() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Automatik GmbH", &["accounting"]).await;

    let create = |number: &str, account_type: &str, tax_code: &str| json!({ "number": number, "name": "Konto", "type": account_type, "tax_code": tax_code });
    let (status, body) = call(
        &h.app,
        "POST",
        "/v1/accounting/accounts",
        &token(&t),
        Some(create("8401", "revenue", "VAT19_OUT")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["tax_code"], "VAT19_OUT");
    let uri = format!("/v1/accounting/accounts/{}", body["id"].as_str().unwrap());

    for (account_type, code) in [("expense", "VAT19_OUT"), ("revenue", "NOPE")] {
        let (status, body) = call(
            &h.app,
            "POST",
            "/v1/accounting/accounts",
            &token(&t),
            Some(create("8402", account_type, code)),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{account_type} {code}: {body}"
        );
    }

    // The type of an automatic account must keep fitting its code.
    let (status, body) = call(
        &h.app,
        "PATCH",
        &uri,
        &token(&t),
        Some(json!({ "type": "expense" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = call(
        &h.app,
        "PATCH",
        &uri,
        &token(&t),
        Some(json!({ "name": "Erlöse Sonder" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tax_code"], "VAT19_OUT");
}
