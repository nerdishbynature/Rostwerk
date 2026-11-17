//! OIDC access tokens through the full router, against a local fake issuer.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::idp::{AUDIENCE, Idp, SigningKey};
use common::{ADMIN, Harness, call, call_with, create_tenant, harness_with, token};
use rostwerk_api::{AuthConfig, OidcVerifier};
use serde_json::{Value, json};

async fn setup(dev_tokens: bool) -> Option<(Harness, Idp)> {
    let idp = Idp::start().await;
    let verifier = OidcVerifier::discover(&idp.issuer, AUDIENCE)
        .unwrap()
        .with_min_refresh(Duration::ZERO);
    let h = harness_with(AuthConfig {
        dev_tokens,
        admin_token: Some(ADMIN.into()),
        oidc: Some(Arc::new(verifier)),
        docs_client_id: Some("rostwerk-docs".into()),
    })
    .await?;
    Some((h, idp))
}

async fn admit(h: &Harness, tenant: &str, sub: &str, kind: &str) {
    let (status, body) = call(
        &h.app,
        "PUT",
        &format!("/admin/tenants/{tenant}/members/{sub}"),
        ADMIN,
        Some(json!({ "kind": kind })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn create_partner(h: &Harness, token: &str, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    call_with(
        &h.app,
        "POST",
        "/v1/partners",
        token,
        headers,
        Some(json!({ "name": "ACME", "kind": "customer" })),
    )
    .await
}

async fn audit_actor(h: &Harness, token: &str, headers: &[(&str, &str)], id: &str) -> Value {
    let (status, page) = call_with(
        &h.app,
        "GET",
        &format!("/v1/audit?entity_id={id}"),
        token,
        headers,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page["items"][0]["actor"].clone()
}

#[tokio::test]
async fn user_token_acts_as_user_in_its_tenant() {
    let Some((h, idp)) = setup(true).await else {
        return;
    };
    let t = create_tenant(&h.app, "OIDC A", &["masterdata"]).await;
    admit(&h, &t, "alice-sub", "user").await;
    let tok = idp.token("alice-sub");

    let (status, partner) = create_partner(&h, &tok, &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{partner}");
    let id = partner["id"].as_str().unwrap();
    assert_eq!(audit_actor(&h, &tok, &[], id).await, "user:alice-sub");
}

#[tokio::test]
async fn agent_membership_acts_as_agent_in_audit_and_outbox() {
    let Some((h, idp)) = setup(true).await else {
        return;
    };
    let t = create_tenant(&h.app, "OIDC Agent", &["masterdata"]).await;
    admit(&h, &t, "svc-agent", "agent").await;
    let tok = idp.token("svc-agent");

    let (status, partner) = create_partner(&h, &tok, &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{partner}");
    let id = partner["id"].as_str().unwrap();
    assert_eq!(audit_actor(&h, &tok, &[], id).await, "agent:svc-agent");

    let actor: String = sqlx::query_scalar("SELECT actor FROM outbox WHERE tenant_id = $1::uuid")
        .bind(&t)
        .fetch_one(&h.admin_pool)
        .await
        .unwrap();
    assert_eq!(actor, "agent:svc-agent");
}

#[tokio::test]
async fn invalid_tokens_are_401() {
    let Some((h, idp)) = setup(true).await else {
        return;
    };
    let t = create_tenant(&h.app, "OIDC 401", &["masterdata"]).await;
    admit(&h, &t, "alice-sub", "user").await;

    let mut wrong_aud = idp.claims("alice-sub");
    wrong_aud["aud"] = json!("other-api");
    let mut expired = idp.claims("alice-sub");
    expired["exp"] = json!(jsonwebtoken::get_current_timestamp() - 3600);
    let mut wrong_iss = idp.claims("alice-sub");
    wrong_iss["iss"] = json!("http://evil.example");
    let forged = SigningKey::generate(&idp.key.kid).sign(&idp.claims("alice-sub"));

    for tok in [
        idp.key.sign(&wrong_aud),
        idp.key.sign(&expired),
        idp.key.sign(&wrong_iss),
        forged,
        "garbage".to_string(),
    ] {
        let (status, body) = call(&h.app, "GET", "/v1/partners", &tok, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{tok}: {body}");
    }
}

#[tokio::test]
async fn valid_token_without_membership_is_403() {
    let Some((h, idp)) = setup(true).await else {
        return;
    };
    let t = create_tenant(&h.app, "OIDC 403", &["masterdata"]).await;
    admit(&h, &t, "alice-sub", "user").await;

    let (status, _) = call(&h.app, "GET", "/v1/partners", &idp.token("mallory"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Alice asking for a tenant she is not in, existing or not: same answer.
    let other = create_tenant(&h.app, "OIDC other", &["masterdata"]).await;
    let random = uuid::Uuid::now_v7().to_string();
    for tenant in [other.as_str(), random.as_str()] {
        let (status, _) = call_with(
            &h.app,
            "GET",
            "/v1/partners",
            &idp.token("alice-sub"),
            &[("x-rostwerk-tenant", tenant)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{tenant}");
    }
}

#[tokio::test]
async fn several_tenants_need_the_tenant_header() {
    let Some((h, idp)) = setup(true).await else {
        return;
    };
    let a = create_tenant(&h.app, "Multi A", &["masterdata"]).await;
    let b = create_tenant(&h.app, "Multi B", &["masterdata"]).await;
    admit(&h, &a, "consultant", "user").await;
    admit(&h, &b, "consultant", "user").await;
    let tok = idp.token("consultant");

    let (status, body) = create_partner(&h, &tok, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = create_partner(&h, &tok, &[("x-rostwerk-tenant", "nope")]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, partner) = create_partner(&h, &tok, &[("x-rostwerk-tenant", &a)]).await;
    assert_eq!(status, StatusCode::CREATED, "{partner}");
    let id = partner["id"].as_str().unwrap();

    // The partner exists in A only.
    let uri = format!("/v1/partners/{id}");
    let (status, _) = call_with(
        &h.app,
        "GET",
        &uri,
        &tok,
        &[("x-rostwerk-tenant", &a)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call_with(
        &h.app,
        "GET",
        &uri,
        &tok,
        &[("x-rostwerk-tenant", &b)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn removed_member_loses_access() {
    let Some((h, idp)) = setup(true).await else {
        return;
    };
    let t = create_tenant(&h.app, "OIDC remove", &["masterdata"]).await;
    admit(&h, &t, "bob", "user").await;
    let tok = idp.token("bob");

    let (status, members) = call(
        &h.app,
        "GET",
        &format!("/admin/tenants/{t}/members"),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(members[0]["subject"], "bob");
    assert_eq!(members[0]["kind"], "user");

    let (status, _) = call(&h.app, "GET", "/v1/partners", &tok, None).await;
    assert_eq!(status, StatusCode::OK);

    let uri = format!("/admin/tenants/{t}/members/bob");
    let (status, _) = call(&h.app, "DELETE", &uri, ADMIN, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&h.app, "DELETE", &uri, ADMIN, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(&h.app, "GET", "/v1/partners", &tok, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admitting_to_unknown_tenant_is_404() {
    let Some((h, _idp)) = setup(true).await else {
        return;
    };
    let uri = format!("/admin/tenants/{}/members/bob", uuid::Uuid::now_v7());
    let (status, _) = call(&h.app, "PUT", &uri, ADMIN, Some(json!({ "kind": "user" }))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rotated_key_is_fetched_on_demand() {
    let Some((h, idp)) = setup(true).await else {
        return;
    };
    let t = create_tenant(&h.app, "OIDC rotate", &["masterdata"]).await;
    admit(&h, &t, "alice-sub", "user").await;

    // Load the JWKS once with the old key.
    let (status, _) = call(&h.app, "GET", "/v1/partners", &idp.token("alice-sub"), None).await;
    assert_eq!(status, StatusCode::OK);

    let new_key = SigningKey::generate("test-key-2");
    idp.publish(&new_key);
    let tok = new_key.sign(&idp.claims("alice-sub"));
    let (status, body) = call(&h.app, "GET", "/v1/partners", &tok, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn dev_tokens_still_work_next_to_oidc_and_only_when_enabled() {
    let Some((h, _idp)) = setup(true).await else {
        return;
    };
    let t = create_tenant(&h.app, "Dev on", &["masterdata"]).await;
    let (status, _) = call(&h.app, "GET", "/v1/partners", &token(&t), None).await;
    assert_eq!(status, StatusCode::OK);

    let Some((h, _idp)) = setup(false).await else {
        return;
    };
    let t = create_tenant(&h.app, "Dev off", &["masterdata"]).await;
    let (status, _) = call(&h.app, "GET", "/v1/partners", &token(&t), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn docs_offer_oidc_sign_in() {
    let Some((h, idp)) = setup(true).await else {
        return;
    };
    let (status, spec) = call(&h.app, "GET", "/openapi.json", "", None).await;
    assert_eq!(status, StatusCode::OK);
    let schemes = &spec["components"]["securitySchemes"];
    assert_eq!(schemes["oidc"]["type"], "openIdConnect");
    assert_eq!(
        schemes["oidc"]["openIdConnectUrl"],
        format!("{}/.well-known/openid-configuration", idp.issuer)
    );
    assert_eq!(schemes["tenant"]["name"], "X-Rostwerk-Tenant");

    let (status, _) = call(&h.app, "GET", "/docs/oauth2-redirect.html", "", None).await;
    assert_eq!(status, StatusCode::OK);
}
