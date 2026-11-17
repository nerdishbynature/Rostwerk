//! Authentication. For now only a development token; OIDC comes later.
//!
//! Dev token format: `Authorization: Bearer dev:<tenant-uuid>:<user>`.
//! It is accepted only when `ROSTWERK_DEV_AUTH=1`.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use core_host::{CoreState, tenants};
use core_kernel::{Actor, Error, Result, TenantCtx, TenantId};
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// Accept `dev:` bearer tokens. Never enable in production.
    pub dev_tokens: bool,
    /// Static token for `/admin`. `None` disables the admin API.
    pub admin_token: Option<String>,
}

impl AuthConfig {
    pub fn from_env() -> Self {
        Self {
            dev_tokens: std::env::var("ROSTWERK_DEV_AUTH").is_ok_and(|v| v == "1"),
            admin_token: std::env::var("ROSTWERK_ADMIN_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
        }
    }
}

pub(crate) fn bearer(req: &Request) -> Option<&str> {
    req.headers()
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn parse_dev_token(token: &str) -> Option<(TenantId, String)> {
    let rest = token.strip_prefix("dev:")?;
    let (tenant, user) = rest.split_once(':')?;
    if user.is_empty() {
        return None;
    }
    Some((TenantId(Uuid::parse_str(tenant).ok()?), user.to_string()))
}

/// Resolves the credential to a [`TenantCtx`] and puts it into the request.
pub(crate) async fn authenticate(
    State((state, config)): State<(CoreState, Arc<AuthConfig>)>,
    mut req: Request,
    next: Next,
) -> Result<Response> {
    if !config.dev_tokens {
        return Err(Error::Unauthorized);
    }
    let (tenant_id, user) = bearer(&req)
        .and_then(parse_dev_token)
        .ok_or(Error::Unauthorized)?;
    let tenant = tenants::find(&state.db, tenant_id)
        .await?
        .ok_or(Error::Unauthorized)?;

    let ctx = TenantCtx::new(tenant_id, Actor::User(user), tenant.modules);
    req.extensions_mut().insert(ctx);
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dev_tokens() {
        let id = Uuid::now_v7();
        let (tenant, user) = parse_dev_token(&format!("dev:{id}:alice")).unwrap();
        assert_eq!(tenant, TenantId(id));
        assert_eq!(user, "alice");
    }

    #[test]
    fn rejects_malformed_dev_tokens() {
        assert!(parse_dev_token("dev:not-a-uuid:alice").is_none());
        assert!(parse_dev_token(&format!("dev:{}:", Uuid::now_v7())).is_none());
        assert!(parse_dev_token(&format!("{}:alice", Uuid::now_v7())).is_none());
    }
}
