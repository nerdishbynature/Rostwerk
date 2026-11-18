//! Custom fields on extensible entities: definitions, validation, system role.

mod common;

use axum::http::StatusCode;
use common::{Harness, call, call_with, create_tenant, harness, harness_with_role, token};
use core_host::SystemRole;
use serde_json::{Value, json};

const PARTNER_FIELDS: &str = "/v1/ext-fields/masterdata.partner";

async fn put_field(
    h: &Harness,
    tenant: &str,
    entity: &str,
    key: &str,
    spec: Value,
) -> (StatusCode, Value) {
    call(
        &h.app,
        "PUT",
        &format!("/v1/ext-fields/{entity}/{key}"),
        &token(tenant),
        Some(spec),
    )
    .await
}

async fn create_partner(h: &Harness, tenant: &str, ext: Option<Value>) -> (StatusCode, Value) {
    let mut body = json!({ "name": "ACME GmbH", "kind": "customer" });
    if let Some(ext) = ext {
        body["ext"] = ext;
    }
    call(&h.app, "POST", "/v1/partners", &token(tenant), Some(body)).await
}

/// Rows of the tenant, counted as the owner role (bypasses RLS).
async fn count(h: &Harness, sql_where: &str, tenant: &str) -> i64 {
    let (n,): (i64,) = sqlx::query_as(&format!(
        "SELECT count(*) FROM {sql_where} AND tenant_id = $1::uuid"
    ))
    .bind(tenant)
    .fetch_one(&h.admin_pool)
    .await
    .unwrap();
    n
}

fn message(body: &Value) -> &str {
    body["message"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn lists_entities_of_enabled_modules() {
    let Some(h) = harness().await else { return };
    let on = create_tenant(&h.app, "With masterdata", &["masterdata"]).await;
    let off = create_tenant(&h.app, "Without modules", &[]).await;

    let (status, entities) = call(&h.app, "GET", "/v1/ext-entities", &token(&on), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        entities,
        json!([
            { "name": "masterdata.article", "module": "masterdata", "label": "Article" },
            { "name": "masterdata.partner", "module": "masterdata", "label": "Business partner" },
        ])
    );

    let (_, entities) = call(&h.app, "GET", "/v1/ext-entities", &token(&off), None).await;
    assert_eq!(entities, json!([]));

    // Unknown entities and those of disabled modules do not exist.
    let spec = json!({ "type": "string", "label": "Note" });
    for (tenant, entity) in [
        (&on, "masterdata.nope"),
        (&on, "sales.order"),
        (&off, "masterdata.partner"),
    ] {
        let (status, body) = put_field(&h, tenant, entity, "note", spec.clone()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{entity}: {body}");
        for suffix in ["", "/schema"] {
            let uri = format!("/v1/ext-fields/{entity}{suffix}");
            let (status, _) = call(&h.app, "GET", &uri, &token(tenant), None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        }
    }
}

#[tokio::test]
async fn put_is_idempotent_and_validates_definitions() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Definitions", &["masterdata"]).await;
    let spec = json!({ "type": "decimal", "label": "Credit limit" });

    let (status, def) = put_field(&h, &t, "masterdata.partner", "credit_limit", spec.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{def}");
    assert_eq!(
        def,
        json!({
            "entity": "masterdata.partner", "key": "credit_limit",
            "type": "decimal", "required": false, "label": "Credit limit",
        })
    );

    // Same definition again: same state, no audit entry, no event.
    let (status, again) = put_field(&h, &t, "masterdata.partner", "credit_limit", spec).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again, def);
    let audited = "audit_log WHERE entity_type = 'ext_field_definitions'";
    let events = "outbox WHERE topic = 'core.ext_field.defined'";
    assert_eq!(count(&h, audited, &t).await, 1);
    assert_eq!(count(&h, events, &t).await, 1);

    // A changed definition replaces the old one.
    let (status, def) = put_field(
        &h,
        &t,
        "masterdata.partner",
        "credit_limit",
        json!({ "type": "decimal", "label": "Kreditlimit", "required": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{def}");
    assert_eq!(def["label"], "Kreditlimit");
    assert_eq!(count(&h, audited, &t).await, 2);
    assert_eq!(count(&h, events, &t).await, 2);

    let (_, fields) = call(&h.app, "GET", PARTNER_FIELDS, &token(&t), None).await;
    assert_eq!(fields, json!([def]));

    // Invalid keys and definitions.
    for (key, spec) in [
        ("Credit", json!({ "type": "string", "label": "x" })),
        ("9lives", json!({ "type": "string", "label": "x" })),
        (&*"a".repeat(64), json!({ "type": "string", "label": "x" })),
        ("segment", json!({ "type": "enum", "label": "Segment" })),
        (
            "segment",
            json!({ "type": "enum", "enum_values": [], "label": "Segment" }),
        ),
        (
            "segment",
            json!({ "type": "enum", "enum_values": ["a", "a"], "label": "Segment" }),
        ),
        (
            "note",
            json!({ "type": "string", "enum_values": ["a"], "label": "Note" }),
        ),
        ("note", json!({ "type": "string", "label": " " })),
    ] {
        let (status, body) = put_field(&h, &t, "masterdata.partner", key, spec.clone()).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{key} {spec}: {body}"
        );
    }
    // Unknown type or property: rejected by the body parser.
    let (status, _) = put_field(
        &h,
        &t,
        "masterdata.partner",
        "x",
        json!({ "type": "float", "label": "x" }),
    )
    .await;
    assert!(status.is_client_error());
    let (status, _) = put_field(
        &h,
        &t,
        "masterdata.partner",
        "x",
        json!({ "type": "string", "label": "x", "id": 1 }),
    )
    .await;
    assert!(status.is_client_error());
}

#[tokio::test]
async fn partner_and_article_writes_validate_ext() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Validation", &["masterdata"]).await;
    for (key, spec) in [
        (
            "credit_limit",
            json!({ "type": "decimal", "label": "Credit limit" }),
        ),
        ("founded", json!({ "type": "date", "label": "Founded" })),
        ("vip", json!({ "type": "boolean", "label": "VIP" })),
        (
            "segment",
            json!({ "type": "enum", "enum_values": ["a", "b"], "required": true, "label": "Segment" }),
        ),
    ] {
        let (status, body) = put_field(&h, &t, "masterdata.partner", key, spec).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    let valid =
        json!({ "segment": "a", "credit_limit": "1500.00", "founded": "1999-04-01", "vip": true });
    let (status, partner) = create_partner(&h, &t, Some(valid.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{partner}");
    assert_eq!(partner["ext"], valid);
    let id = partner["id"].as_str().unwrap();

    for (ext, field) in [
        (None, "ext.segment: is required"),
        (
            Some(json!({ "credit_limit": "1" })),
            "ext.segment: is required",
        ),
        (
            Some(json!({ "segment": "c" })),
            "ext.segment: must be one of",
        ),
        (
            Some(json!({ "segment": "a", "credit_limit": 1500 })),
            "ext.credit_limit:",
        ),
        (
            Some(json!({ "segment": "a", "founded": "1999-02-30" })),
            "ext.founded:",
        ),
        (Some(json!({ "segment": "a", "vip": "yes" })), "ext.vip:"),
        (
            Some(json!({ "segment": "a", "nope": 1 })),
            "ext.nope: unknown field",
        ),
        (Some(json!(["segment"])), "ext must be a JSON object"),
    ] {
        let (status, body) = create_partner(&h, &t, ext.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{ext:?}: {body}");
        assert!(message(&body).starts_with(field), "{ext:?}: {body}");
    }

    // PATCH replaces ext and is validated the same way.
    let uri = format!("/v1/partners/{id}");
    let (status, body) = call(
        &h.app,
        "PATCH",
        &uri,
        &token(&t),
        Some(json!({ "ext": { "vip": false } })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        message(&body).starts_with("ext.segment: is required"),
        "{body}"
    );
    let (status, body) = call(
        &h.app,
        "PATCH",
        &uri,
        &token(&t),
        Some(json!({ "ext": { "segment": "b" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ext"], json!({ "segment": "b" }));
    // Without ext, other changes do not touch it.
    let (status, body) = call(
        &h.app,
        "PATCH",
        &uri,
        &token(&t),
        Some(json!({ "name": "ACME AG" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ext"], json!({ "segment": "b" }));

    // Articles have their own fields.
    let (status, body) = put_field(
        &h,
        &t,
        "masterdata.article",
        "weight_kg",
        json!({ "type": "number", "label": "Weight (kg)" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let article = |ext: Value| {
        json!({
            "number": "A-1", "name": "Bolt", "kind": "goods", "unit": "pcs",
            "sales_price_net": { "amount": "0.10", "currency": "EUR" },
            "tax_category": "standard", "ext": ext,
        })
    };
    let (status, body) = call(
        &h.app,
        "POST",
        "/v1/articles",
        &token(&t),
        Some(article(json!({ "segment": "a" }))),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        message(&body).starts_with("ext.segment: unknown field"),
        "{body}"
    );
    let (status, body) = call(
        &h.app,
        "POST",
        "/v1/articles",
        &token(&t),
        Some(article(json!({ "weight_kg": "heavy" }))),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        message(&body).starts_with("ext.weight_kg: must be a number"),
        "{body}"
    );
    let (status, body) = call(
        &h.app,
        "POST",
        "/v1/articles",
        &token(&t),
        Some(article(json!({ "weight_kg": 0.01 }))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["ext"], json!({ "weight_kg": 0.01 }));
}

#[tokio::test]
async fn schema_endpoint_returns_effective_json_schema() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Schema", &["masterdata"]).await;
    put_field(
        &h,
        &t,
        "masterdata.partner",
        "segment",
        json!({ "type": "enum", "enum_values": ["a", "b"], "required": true, "label": "Segment" }),
    )
    .await;
    put_field(
        &h,
        &t,
        "masterdata.partner",
        "founded",
        json!({ "type": "date", "label": "Founded" }),
    )
    .await;

    let (status, schema) = call(
        &h.app,
        "GET",
        &format!("{PARTNER_FIELDS}/schema"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        schema,
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "masterdata.partner",
            "type": "object",
            "properties": {
                "founded": { "type": "string", "format": "date", "title": "Founded" },
                "segment": { "type": "string", "enum": ["a", "b"], "title": "Segment" },
            },
            "required": ["segment"],
            "additionalProperties": false,
        })
    );

    let (_, empty) = call(
        &h.app,
        "GET",
        "/v1/ext-fields/masterdata.article/schema",
        &token(&t),
        None,
    )
    .await;
    assert_eq!(empty["properties"], json!({}));
}

#[tokio::test]
async fn removed_fields_keep_values_but_are_hidden() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Removal", &["masterdata"]).await;
    let spec = json!({ "type": "string", "label": "Note" });
    put_field(&h, &t, "masterdata.partner", "note", spec.clone()).await;
    let (_, partner) = create_partner(&h, &t, Some(json!({ "note": "call first" }))).await;
    let uri = format!("/v1/partners/{}", partner["id"].as_str().unwrap());

    let (status, removed) = call(
        &h.app,
        "DELETE",
        &format!("{PARTNER_FIELDS}/note"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["key"], "note");
    let (status, _) = call(
        &h.app,
        "DELETE",
        &format!("{PARTNER_FIELDS}/note"),
        &token(&t),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        count(&h, "outbox WHERE topic = 'core.ext_field.deleted'", &t).await,
        1
    );

    // Hidden on read (get, list), still stored.
    let (_, got) = call(&h.app, "GET", &uri, &token(&t), None).await;
    assert_eq!(got["ext"], json!({}));
    let (_, page) = call(&h.app, "GET", "/v1/partners", &token(&t), None).await;
    assert_eq!(page["items"][0]["ext"], json!({}));
    let (stored,): (Value,) = sqlx::query_as("SELECT ext FROM partners WHERE id = $1::uuid")
        .bind(partner["id"].as_str().unwrap())
        .fetch_one(&h.admin_pool)
        .await
        .unwrap();
    assert_eq!(stored, json!({ "note": "call first" }));

    // Defining the field again brings the value back.
    put_field(&h, &t, "masterdata.partner", "note", spec).await;
    let (_, got) = call(&h.app, "GET", &uri, &token(&t), None).await;
    assert_eq!(got["ext"], json!({ "note": "call first" }));
}

#[tokio::test]
async fn definitions_are_per_tenant() {
    let Some(h) = harness().await else { return };
    let a = create_tenant(&h.app, "Tenant A", &["masterdata"]).await;
    let b = create_tenant(&h.app, "Tenant B", &["masterdata"]).await;
    let (status, _) = put_field(
        &h,
        &a,
        "masterdata.partner",
        "credit_limit",
        json!({ "type": "decimal", "label": "Credit limit" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, fields) = call(&h.app, "GET", PARTNER_FIELDS, &token(&b), None).await;
    assert_eq!(fields, json!([]));
    let (status, body) = create_partner(&h, &b, Some(json!({ "credit_limit": "1" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _) = call(
        &h.app,
        "DELETE",
        &format!("{PARTNER_FIELDS}/credit_limit"),
        &token(&b),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // B can define the same key independently.
    let (status, _) = put_field(
        &h,
        &b,
        "masterdata.partner",
        "credit_limit",
        json!({ "type": "number", "label": "Limit" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, fields) = call(&h.app, "GET", PARTNER_FIELDS, &token(&a), None).await;
    assert_eq!(fields[0]["type"], "decimal");
}

#[tokio::test]
async fn dry_run_put_stores_nothing() {
    let Some(h) = harness().await else { return };
    let t = create_tenant(&h.app, "Dry", &["masterdata"]).await;
    let (status, body) = call_with(
        &h.app,
        "PUT",
        &format!("{PARTNER_FIELDS}/note"),
        &token(&t),
        &[("prefer", "dry-run")],
        Some(json!({ "type": "string", "label": "Note" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["events"][0]["topic"], "core.ext_field.defined");
    let (_, fields) = call(&h.app, "GET", PARTNER_FIELDS, &token(&t), None).await;
    assert_eq!(fields, json!([]));
    assert_eq!(count(&h, "outbox WHERE true", &t).await, 0);
}

#[tokio::test]
async fn qas_and_prod_reject_direct_customizing() {
    for role in [SystemRole::Qas, SystemRole::Prod] {
        let Some(h) = harness_with_role(role).await else {
            return;
        };
        let t = create_tenant(&h.app, "Locked", &["masterdata"]).await;

        let (status, body) = put_field(
            &h,
            &t,
            "masterdata.partner",
            "note",
            json!({ "type": "string", "label": "Note" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{role}: {body}");
        assert_eq!(body["error"], "conflict");
        assert!(message(&body).contains("transport"), "{body}");
        assert!(message(&body).contains(&role.to_string()), "{body}");
        let (status, _) = call(
            &h.app,
            "DELETE",
            &format!("{PARTNER_FIELDS}/note"),
            &token(&t),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // Reads and business data writes are unaffected.
        let (status, _) = call(&h.app, "GET", PARTNER_FIELDS, &token(&t), None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            &h.app,
            "GET",
            &format!("{PARTNER_FIELDS}/schema"),
            &token(&t),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = create_partner(&h, &t, None).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(count(&h, "ext_field_definitions WHERE true", &t).await, 0);
    }
}

mod declarations {
    use core_host::{CoreState, ExtensibleEntity, Module, ModuleManifest, ModuleRegistry};
    use utoipa_axum::router::OpenApiRouter;

    struct Declares(&'static [&'static str]);

    impl Module for Declares {
        fn manifest(&self) -> ModuleManifest {
            ModuleManifest {
                id: "decl",
                name: "Decl",
                version: "0",
                description: "",
            }
        }
        fn routes(&self) -> OpenApiRouter<CoreState> {
            OpenApiRouter::new()
        }
        fn extensible_entities(&self) -> Vec<ExtensibleEntity> {
            self.0
                .iter()
                .map(|name| ExtensibleEntity { name, label: "x" })
                .collect()
        }
    }

    #[test]
    fn valid_names_are_accepted() {
        let entities = ModuleRegistry::new()
            .register(Declares(&["decl.thing", "decl.other_2"]))
            .ext_entities();
        assert_eq!(entities.visible(&ctx()).len(), 2);
    }

    fn ctx() -> core_kernel::TenantCtx {
        core_kernel::TenantCtx::new(
            core_kernel::TenantId(uuid::Uuid::nil()),
            core_kernel::Actor::Agent("t".into()),
            ["decl".to_string()],
        )
    }

    #[test]
    #[should_panic(expected = "must be named decl.<entity>")]
    fn names_stay_in_their_module_namespace() {
        let _ = ModuleRegistry::new().register(Declares(&["masterdata.partner"]));
    }

    #[test]
    #[should_panic(expected = "must be named decl.<entity>")]
    fn names_are_lower_case() {
        let _ = ModuleRegistry::new().register(Declares(&["decl.Thing"]));
    }

    #[test]
    #[should_panic(expected = "declared twice")]
    fn names_are_unique() {
        let _ = ModuleRegistry::new().register(Declares(&["decl.thing", "decl.thing"]));
    }
}
