use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::{Result, TenantId};

/// Database handle shared by the core and all modules.
///
/// Modules reach data only through [`TenantCtx::begin`](crate::TenantCtx::begin),
/// which returns a [`TenantTx`]. The app connects as a role without
/// `BYPASSRLS`, so a query outside a tenant transaction sees no tenant rows.
#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Unscoped access for core services (tenant registry, health checks).
    /// Module code must not use this; tenant tables stay protected by RLS anyway.
    pub fn system_pool(&self) -> &PgPool {
        &self.pool
    }

    pub(crate) async fn begin_for_tenant(&self, tenant_id: TenantId) -> Result<TenantTx> {
        let mut tx = self.pool.begin().await?;
        // `true` = local to this transaction; the setting is gone after commit/rollback.
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(tenant_id.to_string())
            .execute(&mut *tx)
            .await?;
        Ok(TenantTx { tx, tenant_id })
    }
}

/// A transaction bound to one tenant. Row-level security filters every query.
pub struct TenantTx {
    tx: Transaction<'static, Postgres>,
    tenant_id: TenantId,
}

impl TenantTx {
    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Connection to run queries on, e.g. `.fetch_all(tx.conn())`.
    pub fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await?;
        Ok(())
    }
}
