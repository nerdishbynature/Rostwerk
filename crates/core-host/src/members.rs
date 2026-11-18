//! Tenant membership: which OIDC subjects may act in which tenant, and as what.
//!
//! The identity provider says *who* someone is (`sub`); Rostwerk decides *where*
//! they may act. Whether a subject acts as a user or as an agent is part of the
//! membership, set by the operator when the identity is admitted.

use chrono::{DateTime, Utc};
use core_kernel::{Actor, Db, Result, TenantId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum MemberKind {
    /// A human, typically signed in via authorization code flow.
    User,
    /// A machine identity, typically a client credentials token.
    Agent,
}

impl MemberKind {
    pub fn actor(self, subject: impl Into<String>) -> Actor {
        match self {
            MemberKind::User => Actor::User(subject.into()),
            MemberKind::Agent => Actor::Agent(subject.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema, sqlx::FromRow)]
pub struct Member {
    pub tenant_id: Uuid,
    /// `sub` claim of the identity at the configured issuer.
    pub subject: String,
    pub kind: MemberKind,
    pub created_at: DateTime<Utc>,
}

/// A tenant the subject belongs to, with what the request needs to run in it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Membership {
    pub tenant_id: Uuid,
    pub modules: Vec<String>,
    pub kind: MemberKind,
}

/// Adds the subject to the tenant or changes its kind. `None` if the tenant does not exist.
pub async fn upsert(
    db: &Db,
    tenant: TenantId,
    subject: &str,
    kind: MemberKind,
) -> Result<Option<Member>> {
    let member = sqlx::query_as!(
        Member,
        r#"INSERT INTO tenant_members (tenant_id, subject, kind)
         SELECT id, $2, $3 FROM tenants WHERE id = $1
         ON CONFLICT (tenant_id, subject) DO UPDATE SET kind = EXCLUDED.kind
         RETURNING tenant_id, subject, kind AS "kind: MemberKind", created_at"#,
        tenant.0,
        subject,
        kind as MemberKind,
    )
    .fetch_optional(db.system_pool())
    .await?;
    Ok(member)
}

/// Removes the subject from the tenant. `false` if it was not a member.
pub async fn remove(db: &Db, tenant: TenantId, subject: &str) -> Result<bool> {
    let result = sqlx::query!(
        "DELETE FROM tenant_members WHERE tenant_id = $1 AND subject = $2",
        tenant.0,
        subject,
    )
    .execute(db.system_pool())
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn list(db: &Db, tenant: TenantId) -> Result<Vec<Member>> {
    let members = sqlx::query_as!(
        Member,
        r#"SELECT tenant_id, subject, kind AS "kind: MemberKind", created_at FROM tenant_members
         WHERE tenant_id = $1 ORDER BY subject"#,
        tenant.0,
    )
    .fetch_all(db.system_pool())
    .await?;
    Ok(members)
}

/// Memberships of a subject, restricted to `tenant` if given.
///
/// Returns at most two rows: enough to tell "none", "exactly one" and "several"
/// apart without loading every tenant of a busy agent.
pub async fn memberships_of(
    db: &Db,
    subject: &str,
    tenant: Option<TenantId>,
) -> Result<Vec<Membership>> {
    let rows = sqlx::query_as!(
        Membership,
        r#"SELECT t.id AS tenant_id, t.modules, m.kind AS "kind: MemberKind"
         FROM tenant_members m JOIN tenants t ON t.id = m.tenant_id
         WHERE m.subject = $1 AND ($2::uuid IS NULL OR m.tenant_id = $2)
         ORDER BY t.id
         LIMIT 2"#,
        subject,
        tenant.map(|t| t.0),
    )
    .fetch_all(db.system_pool())
    .await?;
    Ok(rows)
}
