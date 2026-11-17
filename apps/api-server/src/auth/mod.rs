//! Authentication: turns a bearer credential into a [`TenantCtx`].
//!
//! Two kinds of credentials are accepted:
//!
//! - **OIDC access tokens** (JWT) of the configured issuer, when
//!   `ROSTWERK_OIDC_ISSUER` is set. The token proves *who* is calling (`sub`);
//!   the `tenant_members` table decides *where* they may act and whether they
//!   act as a user or as an agent. A subject in several tenants picks one with
//!   the `X-Rostwerk-Tenant` header.
//! - **Dev tokens** `dev:<tenant-uuid>:<user>`, only when `ROSTWERK_DEV_AUTH=1`.
//!
//! Status codes: no or invalid credential = 401; valid credential but not a
//! member of the (requested) tenant = 403; several tenants and no header = 400.

pub mod oidc;

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use core_host::{CoreState, members, tenants};
use core_kernel::{Actor, Error, Result, TenantCtx, TenantId};
use uuid::Uuid;

pub use oidc::{DocsLogin, OidcVerifier};

/// Header that selects the tenant when the caller belongs to several.
pub const TENANT_HEADER: &str = "x-rostwerk-tenant";

#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// Accept `dev:` bearer tokens. Never enable in production.
    pub dev_tokens: bool,
    /// Static token for `/admin`. `None` disables the admin API.
    pub admin_token: Option<String>,
    /// Verifier for OIDC access tokens. `None` disables OIDC.
    pub oidc: Option<Arc<OidcVerifier>>,
    /// Browser sign-in for `/docs` (Swagger UI). `None` hides it.
    pub docs_login: Option<DocsLogin>,
}

impl AuthConfig {
    /// Reads `ROSTWERK_DEV_AUTH`, `ROSTWERK_ADMIN_TOKEN`, `ROSTWERK_OIDC_ISSUER`,
    /// `ROSTWERK_OIDC_AUDIENCE` and `ROSTWERK_OIDC_DOCS_CLIENT_ID`.
    ///
    /// With a docs client id, the issuer's sign-in endpoints are looked up now.
    /// If the issuer is unreachable, the server still starts: `/docs` then
    /// offers only the plain bearer field, and token checks retry on demand.
    pub async fn from_env() -> anyhow::Result<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let oidc = match (var("ROSTWERK_OIDC_ISSUER"), var("ROSTWERK_OIDC_AUDIENCE")) {
            (Some(issuer), Some(audience)) => {
                Some(Arc::new(OidcVerifier::discover(issuer, audience)?))
            }
            (None, None) => None,
            _ => anyhow::bail!(
                "set both ROSTWERK_OIDC_ISSUER and ROSTWERK_OIDC_AUDIENCE, or neither"
            ),
        };
        let docs_login = match (&oidc, var("ROSTWERK_OIDC_DOCS_CLIENT_ID")) {
            (Some(verifier), Some(client_id)) => match verifier.docs_login(&client_id).await {
                Ok(login) => Some(login),
                Err(err) => {
                    tracing::warn!(error = %err, "issuer unreachable, /docs without sign-in");
                    None
                }
            },
            _ => None,
        };
        Ok(Self {
            dev_tokens: var("ROSTWERK_DEV_AUTH").is_some_and(|v| v == "1"),
            admin_token: var("ROSTWERK_ADMIN_TOKEN"),
            oidc,
            docs_login,
        })
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

/// The tenant the caller asks for, if any. A malformed header is a client error.
fn requested_tenant(req: &Request) -> Result<Option<TenantId>> {
    let Some(value) = req.headers().get(TENANT_HEADER) else {
        return Ok(None);
    };
    value
        .to_str()
        .ok()
        .and_then(|v| Uuid::parse_str(v.trim()).ok())
        .map(|id| Some(TenantId(id)))
        .ok_or_else(|| Error::BadRequest(format!("{TENANT_HEADER} must be a tenant id (UUID)")))
}

/// Resolves the credential to a [`TenantCtx`] and puts it into the request.
pub(crate) async fn authenticate(
    State((state, config)): State<(CoreState, Arc<AuthConfig>)>,
    mut req: Request,
    next: Next,
) -> Result<Response> {
    let token = bearer(&req).ok_or(Error::Unauthorized)?;
    let ctx = if config.dev_tokens && token.starts_with("dev:") {
        dev_context(&state, token).await?
    } else if let Some(verifier) = &config.oidc {
        oidc_context(&state, verifier, token, requested_tenant(&req)?).await?
    } else {
        return Err(Error::Unauthorized);
    };
    req.extensions_mut().insert(ctx);
    Ok(next.run(req).await)
}

async fn dev_context(state: &CoreState, token: &str) -> Result<TenantCtx> {
    let (tenant_id, user) = parse_dev_token(token).ok_or(Error::Unauthorized)?;
    let tenant = tenants::find(&state.db, tenant_id)
        .await?
        .ok_or(Error::Unauthorized)?;
    Ok(TenantCtx::new(tenant_id, Actor::User(user), tenant.modules))
}

async fn oidc_context(
    state: &CoreState,
    verifier: &OidcVerifier,
    token: &str,
    requested: Option<TenantId>,
) -> Result<TenantCtx> {
    let claims = verifier.verify(token).await.map_err(|err| {
        match err {
            oidc::VerifyError::Keys(_) => tracing::warn!(error = %err, "OIDC token rejected"),
            _ => tracing::debug!(error = %err, "OIDC token rejected"),
        }
        Error::Unauthorized
    })?;

    let mut found = members::memberships_of(&state.db, &claims.sub, requested).await?;
    let membership = match found.len() {
        // Authenticated, but not admitted here. Unknown and foreign tenants look
        // the same, so tenant ids cannot be probed.
        0 => return Err(Error::Forbidden),
        1 => found.remove(0),
        _ => {
            return Err(Error::BadRequest(format!(
                "member of several tenants, select one with {TENANT_HEADER}"
            )));
        }
    };
    Ok(TenantCtx::new(
        TenantId(membership.tenant_id),
        membership.kind.actor(claims.sub),
        membership.modules,
    ))
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
