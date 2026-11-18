//! Accounting module: chart of accounts and the immutable double-entry journal.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::{Harness, call, call_with, create_tenant, harness, token};
use serde_json::{Value, json};
use sqlx::PgPool;

async fn account(app: &Router, tenant: &str, number: &str, account_type: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        "/v1/accounting/accounts",
        &token(tenant),
        Some(json!({ "number": number, "name": format!("Konto {number}"), "type": account_type })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_string()
}

fn line(account: &str, side: &str, amount: &str) -> Value {
    json!({ "account_id": account, "side": side, "amount": { "amount": amount, "currency": "EUR" } })
}

/// A balanced draft: `amount` from `debit` to `credit`.
fn booking(debit: &str, credit: &str, amount: &str, date: &str) -> Value {
    json!({
        "booking_date": date,
        "document_ref": "RE-2026-001",
        "description": "Wareneinkauf",
        "lines": [line(debit, "debit", amount), line(credit, "credit", amount)],
    })
}

async fn draft(app: &Router, tenant: &str, body: Value) -> String {
    let (status, created) = call(
        app,
        "POST",
        "/v1/accounting/entries",
        &token(tenant),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["status"], "draft");
    created["id"].as_str().unwrap().to_string()
}

async fn post(app: &Router, tenant: &str, entry: &str) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/v1/accounting/entries/{entry}/post"),
        &token(tenant),
        None,
    )
    .await
}

async fn posted(app: &Router, tenant: &str, body: Value) -> Value {
    let id = draft(app, tenant, body).await;
    let (status, entry) = post(app, tenant, &id).await;
    assert_eq!(status, StatusCode::OK, "{entry}");
    entry
}

async fn balance(app: &Router, tenant: &str, account: &str, query: &str) -> Value {
    let (status, body) = call(
        app,
        "GET",
        &format!("/v1/accounting/accounts/{account}/balance{query}"),
        &token(tenant),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// Tenant with the accounting module and two accounts: (tenant, expense, bank).
async fn setup(h: &Harness, name: &str) -> (String, String, String) {
    let t = create_tenant(&h.app, name, &["accounting"]).await;
    let expense = account(&h.app, &t, "3400", "expense").await;
    let bank = account(&h.app, &t, "1200", "asset").await;
    (t, expense, bank)
}

#[tokio::test]
async fn accounts_crud() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Accounts CRUD", &["accounting"]).await;

    let (status, created) = call(
        &h.app,
        "POST",
        "/v1/accounting/accounts",
        &token(&t),
        Some(json!({ "number": " 1200 ", "name": "Bank", "type": "asset" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["number"], "1200");
    assert_eq!(created["type"], "asset");
    let id = created["id"].as_str().unwrap();
    let uri = format!("/v1/accounting/accounts/{id}");

    let (status, fetched) = call(&h.app, "GET", &uri, &token(&t), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched, created);

    // Duplicate numbers conflict.
    let (status, body) = call(
        &h.app,
        "POST",
        "/v1/accounting/accounts",
        &token(&t),
        Some(json!({ "number": "1200", "name": "Bank 2", "type": "asset" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (status, body) = call(
        &h.app,
        "POST",
        "/v1/accounting/accounts",
        &token(&t),
        Some(json!({ "number": "1", "name": "x", "type": "income" })),
    )
    .await;
    assert!(status.is_client_error(), "{status} {body}");

    let (status, updated) = call(
        &h.app,
        "PATCH",
        &uri,
        &token(&t),
        Some(json!({ "name": "Bank Sparkasse", "number": "1210", "type": "liability" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["name"], "Bank Sparkasse");
    assert_eq!(updated["number"], "1210");
    assert_eq!(updated["type"], "liability");

    let (status, page) = call(&h.app, "GET", "/v1/accounting/accounts", &token(&t), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"], json!([updated]));
    assert_eq!(page["next_cursor"], Value::Null);

    let (status, deleted) = call(&h.app, "DELETE", &uri, &token(&t), None).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["id"], created["id"]);
    let (status, _) = call(&h.app, "GET", &uri, &token(&t), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&h.app, "DELETE", &uri, &token(&t), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn accounts_in_use_cannot_be_deleted_or_renumbered() {
    let Some(h) = harness().await else { return };
    let (t, expense, bank) = setup(&h, "Accounts in use").await;

    // A draft already blocks deletion.
    let entry = draft(&h.app, &t, booking(&expense, &bank, "10", "2026-11-02")).await;
    let (status, body) = call(
        &h.app,
        "DELETE",
        &format!("/v1/accounting/accounts/{expense}"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Before posting, number and type may still change.
    let uri = format!("/v1/accounting/accounts/{bank}");
    let (status, body) = call(
        &h.app,
        "PATCH",
        &uri,
        &token(&t),
        Some(json!({ "number": "1210" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, _) = post(&h.app, &t, &entry).await;
    assert_eq!(status, StatusCode::OK);
    for change in [json!({ "number": "1220" }), json!({ "type": "liability" })] {
        let (status, body) = call(&h.app, "PATCH", &uri, &token(&t), Some(change)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }
    // Renaming is fine, and so is "changing" to the current values.
    let (status, body) = call(
        &h.app,
        "PATCH",
        &uri,
        &token(&t),
        Some(json!({ "name": "Bank", "number": "1210", "type": "asset" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = call(&h.app, "DELETE", &uri, &token(&t), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn drafts_are_editable_and_posting_numbers_gaplessly() {
    let Some(h) = harness().await else { return };
    let (t, expense, bank) = setup(&h, "Journal posting").await;

    let id = draft(
        &h.app,
        &t,
        json!({ "description": "  Büromaterial ", "lines": [line(&expense, "debit", "50")] }),
    )
    .await;
    let uri = format!("/v1/accounting/entries/{id}");
    let (status, entry) = call(&h.app, "GET", &uri, &token(&t), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(entry["description"], "Büromaterial");
    assert_eq!(entry["sequence_no"], Value::Null);
    assert_eq!(entry["lines"].as_array().unwrap().len(), 1);

    let (status, updated) = call(
        &h.app,
        "PATCH",
        &uri,
        &token(&t),
        Some(json!({
            "booking_date": "2026-11-03",
            "document_ref": "B-17",
            "lines": [line(&expense, "debit", "59.50"), line(&bank, "credit", "59.50")],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["description"], "Büromaterial");
    let lines = updated["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["line_no"], 1);
    assert_eq!(lines[0]["side"], "debit");
    assert_eq!(lines[0]["amount"]["amount"], "59.5000");
    assert_eq!(lines[1]["line_no"], 2);
    assert_eq!(lines[1]["account_id"], bank.as_str());

    // A dry-run posting shows the number but does not use it up.
    let (status, preview) = call_with(
        &h.app,
        "POST",
        &format!("{uri}/post"),
        &token(&t),
        &[("prefer", "dry-run")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["result"]["sequence_no"], 1);
    assert_eq!(preview["events"][0]["topic"], "accounting.entry.posted");

    let (status, first) = post(&h.app, &t, &id).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["status"], "posted");
    assert_eq!(first["sequence_no"], 1);
    assert!(first["posted_at"].is_string());

    let second = posted(&h.app, &t, booking(&expense, &bank, "1", "2026-11-04")).await;
    assert_eq!(second["sequence_no"], 2);

    // Numbering is per tenant.
    let (other, other_expense, other_bank) = setup(&h, "Journal posting other").await;
    let entry = posted(
        &h.app,
        &other,
        booking(&other_expense, &other_bank, "1", "2026-11-04"),
    )
    .await;
    assert_eq!(entry["sequence_no"], 1);

    // Posted entries are final.
    let (status, _) = post(&h.app, &t, &id).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(
        &h.app,
        "PATCH",
        &uri,
        &token(&t),
        Some(json!({ "description": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(&h.app, "DELETE", &uri, &token(&t), None).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Drafts can be deleted.
    let gone = draft(&h.app, &t, booking(&expense, &bank, "5", "2026-11-05")).await;
    let gone_uri = format!("/v1/accounting/entries/{gone}");
    let (status, _) = call(&h.app, "DELETE", &gone_uri, &token(&t), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&h.app, "GET", &gone_uri, &token(&t), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, page) = call(
        &h.app,
        "GET",
        "/v1/accounting/entries?status=posted",
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let numbers: Vec<_> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["sequence_no"].as_i64().unwrap())
        .collect();
    assert_eq!(numbers, [1, 2]);
    let (_, page) = call(
        &h.app,
        "GET",
        "/v1/accounting/entries?status=draft",
        &token(&t),
        None,
    )
    .await;
    assert_eq!(page["items"], json!([]));
}

#[tokio::test]
async fn posting_requires_a_complete_balanced_entry() {
    let Some(h) = harness().await else { return };
    let (t, expense, bank) = setup(&h, "Journal validation").await;

    let usd = json!({ "account_id": bank, "side": "credit", "amount": { "amount": "10", "currency": "USD" } });
    let cases = [
        (
            "unbalanced",
            booking(&expense, &bank, "10", "2026-11-01"),
            Some(json!([
                line(&expense, "debit", "10"),
                line(&bank, "credit", "9.99")
            ])),
        ),
        (
            "no lines",
            booking(&expense, &bank, "10", "2026-11-01"),
            Some(json!([])),
        ),
        (
            "one side",
            booking(&expense, &bank, "10", "2026-11-01"),
            Some(json!([
                line(&expense, "debit", "10"),
                line(&bank, "debit", "10")
            ])),
        ),
        (
            "mixed currency",
            booking(&expense, &bank, "10", "2026-11-01"),
            Some(json!([line(&expense, "debit", "10"), usd])),
        ),
        (
            "no booking date",
            json!({ "document_ref": "X", "lines": [line(&expense, "debit", "1"), line(&bank, "credit", "1")] }),
            None,
        ),
        (
            "no document",
            json!({ "booking_date": "2026-11-01", "lines": [line(&expense, "debit", "1"), line(&bank, "credit", "1")] }),
            None,
        ),
    ];
    for (case, body, lines) in cases {
        let id = draft(&h.app, &t, body).await;
        if let Some(lines) = lines {
            let (status, body) = call(
                &h.app,
                "PATCH",
                &format!("/v1/accounting/entries/{id}"),
                &token(&t),
                Some(json!({ "lines": lines })),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{case}: {body}");
        }
        let (status, body) = post(&h.app, &t, &id).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{case}: {body}");
    }

    // Invalid lines are rejected already while drafting.
    let missing = uuid::Uuid::now_v7().to_string();
    for (case, bad) in [
        ("zero", line(&expense, "debit", "0")),
        ("negative", line(&expense, "debit", "-1")),
        ("too precise", line(&expense, "debit", "0.00001")),
        ("unknown account", line(&missing, "debit", "1")),
    ] {
        let (status, body) = call(
            &h.app,
            "POST",
            "/v1/accounting/entries",
            &token(&t),
            Some(json!({ "lines": [bad] })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{case}: {body}");
    }
    let (status, _) = call(
        &h.app,
        "POST",
        "/v1/accounting/entries",
        &token(&t),
        Some(json!({ "document_ref": "  " })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Failed postings use up no numbers.
    let ok = posted(&h.app, &t, booking(&expense, &bank, "10", "2026-11-01")).await;
    assert_eq!(ok["sequence_no"], 1);
}

#[tokio::test]
async fn reversal_mirrors_and_references_the_original() {
    let Some(h) = harness().await else { return };
    let (t, expense, bank) = setup(&h, "Journal reversal").await;
    let original = posted(&h.app, &t, booking(&expense, &bank, "119", "2026-11-02")).await;
    let id = original["id"].as_str().unwrap();
    let uri = format!("/v1/accounting/entries/{id}/reverse");

    let (status, reversal) = call(
        &h.app,
        "POST",
        &uri,
        &token(&t),
        Some(json!({ "booking_date": "2026-11-10" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reversal}");
    assert_eq!(reversal["status"], "posted");
    assert_eq!(reversal["sequence_no"], 2);
    assert_eq!(reversal["reverses_id"], id);
    assert_eq!(reversal["booking_date"], "2026-11-10");
    assert_eq!(reversal["document_ref"], "RE-2026-001");
    assert_eq!(reversal["description"], "Storno of #1: Wareneinkauf");
    let lines = reversal["lines"].as_array().unwrap();
    assert_eq!(lines[0]["account_id"], expense.as_str());
    assert_eq!(lines[0]["side"], "credit");
    assert_eq!(lines[0]["amount"], original["lines"][0]["amount"]);
    assert_eq!(lines[1]["account_id"], bank.as_str());
    assert_eq!(lines[1]["side"], "debit");

    let (_, original) = call(
        &h.app,
        "GET",
        &format!("/v1/accounting/entries/{id}"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(original["reversed_by_id"], reversal["id"]);

    // Once only, and only posted entries.
    let (status, _) = call(&h.app, "POST", &uri, &token(&t), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let d = draft(&h.app, &t, booking(&expense, &bank, "1", "2026-11-02")).await;
    let (status, _) = call(
        &h.app,
        "POST",
        &format!("/v1/accounting/entries/{d}/reverse"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let missing = uuid::Uuid::now_v7();
    let (status, _) = call(
        &h.app,
        "POST",
        &format!("/v1/accounting/entries/{missing}/reverse"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Reversed: the balance is back to zero after the reversal date.
    let before = balance(&h.app, &t, &expense, "?until=2026-11-09").await;
    assert_eq!(before["balances"][0]["balance"]["amount"], "119.0000");
    let after = balance(&h.app, &t, &expense, "").await;
    assert_eq!(after["balances"][0]["balance"]["amount"], "0.0000");
}

#[tokio::test]
async fn balance_sums_posted_lines_until_a_date() {
    let Some(h) = harness().await else { return };
    let (t, expense, bank) = setup(&h, "Journal balance").await;

    let empty = balance(&h.app, &t, &bank, "").await;
    assert_eq!(empty["balances"], json!([]));

    posted(&h.app, &t, booking(&expense, &bank, "100", "2026-10-31")).await;
    posted(&h.app, &t, booking(&expense, &bank, "20.50", "2026-11-01")).await;
    posted(&h.app, &t, booking(&bank, &expense, "5", "2026-11-15")).await;
    // Drafts do not count.
    draft(&h.app, &t, booking(&expense, &bank, "1000", "2026-11-01")).await;

    let all = balance(&h.app, &t, &bank, "").await;
    assert_eq!(all["account_id"], bank.as_str());
    assert_eq!(all["until"], Value::Null);
    assert_eq!(
        all["balances"],
        json!([{
            "debit": { "amount": "5.0000", "currency": "EUR" },
            "credit": { "amount": "120.5000", "currency": "EUR" },
            "balance": { "amount": "-115.5000", "currency": "EUR" },
        }])
    );

    let until = balance(&h.app, &t, &expense, "?until=2026-11-01").await;
    assert_eq!(until["until"], "2026-11-01");
    assert_eq!(until["balances"][0]["balance"]["amount"], "120.5000");
    let until = balance(&h.app, &t, &expense, "?until=2026-10-30").await;
    assert_eq!(until["balances"], json!([]));

    let missing = uuid::Uuid::now_v7();
    let (status, _) = call(
        &h.app,
        "GET",
        &format!("/v1/accounting/accounts/{missing}/balance"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn emits_events_and_audit() {
    let Some(h) = harness().await else { return };
    let (t, expense, bank) = setup(&h, "Journal events").await;
    let entry = posted(&h.app, &t, booking(&expense, &bank, "10", "2026-11-01")).await;

    let events: Vec<(String, Value)> =
        sqlx::query_as("SELECT topic, payload FROM outbox WHERE tenant_id = $1::uuid ORDER BY id")
            .bind(&t)
            .fetch_all(&h.admin_pool)
            .await
            .unwrap();
    let topics: Vec<_> = events.iter().map(|(topic, _)| topic.as_str()).collect();
    assert_eq!(
        topics,
        [
            "accounting.account.created",
            "accounting.account.created",
            "accounting.entry.created",
            "accounting.entry.posted",
        ]
    );
    assert_eq!(events[3].1, entry);

    let id = entry["id"].as_str().unwrap();
    let (status, audit) = call(
        &h.app,
        "GET",
        &format!("/v1/audit?entity_id={id}"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = audit["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{audit}");
    assert_eq!(items[0]["entity_type"], "journal_entries");
    assert_eq!(items[0]["action"], "insert");
    assert_eq!(items[1]["action"], "update");
    assert_eq!(items[1]["after"]["status"], "posted");

    let line_id = entry["lines"][0]["id"].as_str().unwrap();
    let (_, audit) = call(
        &h.app,
        "GET",
        &format!("/v1/audit?entity_id={line_id}"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(audit["items"][0]["entity_type"], "journal_lines");
}

/// Runs `sql` as the app role inside a transaction for `tenant`.
async fn raw(pool: &PgPool, tenant: &str, sql: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant)
        .execute(&mut *tx)
        .await?;
    sqlx::raw_sql(sql).execute(&mut *tx).await?;
    tx.commit().await
}

#[tokio::test]
async fn database_rejects_changes_to_posted_entries() {
    let Some(h) = harness().await else { return };
    let (t, expense, bank) = setup(&h, "Journal immutable").await;
    let entry = posted(&h.app, &t, booking(&expense, &bank, "10", "2026-11-01")).await;
    let draft_id = draft(&h.app, &t, booking(&expense, &bank, "10", "2026-11-01")).await;
    let id = entry["id"].as_str().unwrap();
    let line = entry["lines"][0]["id"].as_str().unwrap();
    let tenant = &t;

    const POSTED: &str = "is posted and cannot be changed";
    const LINES: &str = "lines of posted journal entry";
    const SEQUENCE: &str = "only advance by one";
    let attempts = [
        (format!("UPDATE journal_entries SET description = 'changed' WHERE id = '{id}'"), POSTED),
        (
            format!("UPDATE journal_entries SET status = 'draft', sequence_no = NULL, posted_at = NULL WHERE id = '{id}'"),
            POSTED,
        ),
        (format!("DELETE FROM journal_entries WHERE id = '{id}'"), POSTED),
        (format!("UPDATE journal_lines SET amount = 1 WHERE id = '{line}'"), LINES),
        (format!("UPDATE journal_lines SET entry_id = '{draft_id}' WHERE id = '{line}'"), LINES),
        (format!("DELETE FROM journal_lines WHERE id = '{line}'"), LINES),
        (
            format!(
                "INSERT INTO journal_lines (id, tenant_id, entry_id, line_no, account_id, side, amount, currency)
                 VALUES (gen_random_uuid(), '{tenant}', '{id}', 3, '{expense}', 'debit', 1, 'EUR')"
            ),
            "lines cannot be added to posted journal entry",
        ),
        // Posted entries cannot be inserted directly, bypassing the checks.
        (
            format!(
                "INSERT INTO journal_entries (id, tenant_id, status, sequence_no, booking_date, document_ref, posted_at)
                 VALUES (gen_random_uuid(), '{tenant}', 'posted', 2, '2026-11-01', 'X', now())"
            ),
            "created as drafts",
        ),
        // Posting must take the next number and balance.
        (
            format!("UPDATE journal_entries SET status = 'posted', sequence_no = 7, posted_at = now() WHERE id = '{draft_id}'"),
            "must take the next sequence number",
        ),
        (
            format!(
                "UPDATE journal_sequences SET last_no = last_no + 1;
                 UPDATE journal_lines SET amount = 11 WHERE entry_id = '{draft_id}' AND line_no = 1;
                 UPDATE journal_entries SET status = 'posted', sequence_no = 2, posted_at = now() WHERE id = '{draft_id}'"
            ),
            "is not balanced",
        ),
        // Numbers cannot be skipped, reset or taken without a posted entry.
        ("UPDATE journal_sequences SET last_no = last_no + 2".to_string(), SEQUENCE),
        ("UPDATE journal_sequences SET last_no = last_no + 1".to_string(), "not used by a posted entry"),
        ("DELETE FROM journal_sequences".to_string(), SEQUENCE),
        ("TRUNCATE journal_lines".to_string(), "permission denied"),
    ];
    for (sql, expected) in &attempts {
        let err = raw(&h.app_pool, tenant, sql)
            .await
            .expect_err(&format!("succeeded as rostwerk_app: {sql}"));
        assert!(err.to_string().contains(expected), "{sql}: {err}");
    }

    let (_, after) = call(
        &h.app,
        "GET",
        &format!("/v1/accounting/entries/{id}"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(after, entry);

    // Drafts stay editable, and the next posting still gets number 2.
    raw(
        &h.app_pool,
        tenant,
        &format!("UPDATE journal_entries SET description = 'edited' WHERE id = '{draft_id}'"),
    )
    .await
    .expect("drafts are editable");
    let (status, next) = post(&h.app, &t, &draft_id).await;
    assert_eq!(status, StatusCode::OK, "{next}");
    assert_eq!(next["sequence_no"], 2);
}

#[tokio::test]
async fn tenants_and_module_are_isolated() {
    let Some(h) = harness().await else { return };
    let (a, expense, bank) = setup(&h, "Accounting iso A").await;
    let b = create_tenant(&h.app, "Accounting iso B", &["accounting"]).await;
    let entry = posted(&h.app, &a, booking(&expense, &bank, "10", "2026-11-01")).await;
    let id = entry["id"].as_str().unwrap();

    for uri in [
        format!("/v1/accounting/accounts/{bank}"),
        format!("/v1/accounting/accounts/{bank}/balance"),
        format!("/v1/accounting/entries/{id}"),
    ] {
        let (status, _) = call(&h.app, "GET", &uri, &token(&b), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    let (status, _) = call(
        &h.app,
        "POST",
        &format!("/v1/accounting/entries/{id}/reverse"),
        &token(&b),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Another tenant's accounts cannot be booked on.
    let (status, _) = call(
        &h.app,
        "POST",
        "/v1/accounting/entries",
        &token(&b),
        Some(booking(&expense, &bank, "1", "2026-11-01")),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, page) = call(&h.app, "GET", "/v1/accounting/entries", &token(&b), None).await;
    assert_eq!(page["items"], json!([]));

    // Without the module enabled, the routes do not exist.
    let c = create_tenant(&h.app, "Accounting off", &["masterdata"]).await;
    let (status, _) = call(&h.app, "GET", "/v1/accounting/accounts", &token(&c), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    for table in [
        "accounts",
        "journal_entries",
        "journal_lines",
        "journal_sequences",
    ] {
        let (count,): (i64,) = sqlx::query_as(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&h.app_pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "{table} visible without tenant");
    }
}

#[tokio::test]
async fn concurrent_postings_get_distinct_gapless_numbers() {
    let Some(h) = harness().await else { return };
    let (t, expense, bank) = setup(&h, "Journal concurrency").await;
    let mut drafts = Vec::new();
    for i in 1..=8 {
        let amount = i.to_string();
        drafts.push(draft(&h.app, &t, booking(&expense, &bank, &amount, "2026-11-01")).await);
    }

    let postings = drafts.iter().map(|id| post(&h.app, &t, id));
    let mut numbers: Vec<i64> = futures_util::future::join_all(postings)
        .await
        .into_iter()
        .map(|(status, entry)| {
            assert_eq!(status, StatusCode::OK, "{entry}");
            entry["sequence_no"].as_i64().unwrap()
        })
        .collect();
    numbers.sort();
    assert_eq!(numbers, (1..=8).collect::<Vec<_>>());
}
