//! Tenant registry: which tenants exist and which modules they have enabled.

use chrono::{DateTime, Utc};
use core_kernel::{Db, Result, TenantId, new_id};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, ToSchema, sqlx::FromRow)]
pub struct Tenant {
    pub id: Uuid,
    pub name: String,
    pub modules: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct NewTenant {
    pub name: String,
    #[serde(default)]
    pub modules: Vec<String>,
}

pub async fn create(db: &Db, new: NewTenant) -> Result<Tenant> {
    let tenant = sqlx::query_as::<_, Tenant>(
        "INSERT INTO tenants (id, name, modules) VALUES ($1, $2, $3)
         RETURNING id, name, modules, created_at",
    )
    .bind(new_id())
    .bind(new.name.trim())
    .bind(&new.modules)
    .fetch_one(db.system_pool())
    .await?;
    Ok(tenant)
}

pub async fn find(db: &Db, id: TenantId) -> Result<Option<Tenant>> {
    let tenant = sqlx::query_as::<_, Tenant>(
        "SELECT id, name, modules, created_at FROM tenants WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(db.system_pool())
    .await?;
    Ok(tenant)
}

pub async fn set_modules(db: &Db, id: TenantId, modules: &[String]) -> Result<Option<Tenant>> {
    let tenant = sqlx::query_as::<_, Tenant>(
        "UPDATE tenants SET modules = $2 WHERE id = $1
         RETURNING id, name, modules, created_at",
    )
    .bind(id)
    .bind(modules)
    .fetch_optional(db.system_pool())
    .await?;
    Ok(tenant)
}
