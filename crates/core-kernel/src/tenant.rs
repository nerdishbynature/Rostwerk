use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::{Db, Error, Result, TenantId, TenantTx};

/// Who is acting. Agents are first-class identities, not users in disguise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    User(String),
    Agent(String),
}

/// Context of the current request: which tenant, who acts, which modules are on.
///
/// It is derived from the verified credential by the core, never from client
/// input, and inserted into the request extensions. Handlers take it as an
/// extractor; without it there is no way to open a [`TenantTx`].
#[derive(Debug, Clone)]
pub struct TenantCtx {
    tenant_id: TenantId,
    actor: Actor,
    modules: Arc<HashSet<String>>,
}

impl TenantCtx {
    pub fn new(
        tenant_id: TenantId,
        actor: Actor,
        modules: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            tenant_id,
            actor,
            modules: Arc::new(modules.into_iter().collect()),
        }
    }

    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    pub fn has_module(&self, module_id: &str) -> bool {
        self.modules.contains(module_id)
    }

    /// Opens a transaction scoped to this tenant (Postgres RLS is armed).
    pub async fn begin(&self, db: &Db) -> Result<TenantTx> {
        db.begin_for_tenant(self.tenant_id).await
    }
}

impl<S: Send + Sync> FromRequestParts<S> for TenantCtx {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self> {
        parts
            .extensions
            .get::<TenantCtx>()
            .cloned()
            .ok_or(Error::Unauthorized)
    }
}
