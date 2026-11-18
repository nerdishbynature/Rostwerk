use serde::Serialize;
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::{Actor, Error, Result, TenantId, new_id};

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

    pub(crate) async fn begin_for_tenant(
        &self,
        tenant_id: TenantId,
        actor: &Actor,
    ) -> Result<TenantTx> {
        let mut tx = self.pool.begin().await?;
        // `true` = local to this transaction; the settings are gone after commit/rollback.
        // app.tenant_id arms RLS, app.actor is read by the audit trigger.
        sqlx::query!(
            r#"SELECT set_config('app.tenant_id', $1, true) AS "tenant_id",
                      set_config('app.actor', $2, true) AS "actor""#,
            tenant_id.to_string(),
            actor.to_string(),
        )
        .fetch_one(&mut *tx)
        .await?;
        Ok(TenantTx {
            tx,
            tenant_id,
            actor: actor.to_string(),
            events: Vec::new(),
        })
    }

    /// Cross-tenant transaction for core background jobs (outbox relay).
    /// Only tables whose policy allows `app.system` become visible.
    pub async fn begin_system(&self) -> Result<SystemTx> {
        let mut tx = self.pool.begin().await?;
        sqlx::query!("SELECT set_config('app.system', 'on', true)")
            .fetch_one(&mut *tx)
            .await?;
        Ok(SystemTx { tx })
    }
}

/// A transaction bound to one tenant. Row-level security filters every query,
/// and every change to an audited table is journaled with the acting identity.
pub struct TenantTx {
    tx: Transaction<'static, Postgres>,
    tenant_id: TenantId,
    actor: String,
    events: Vec<RecordedEvent>,
}

/// An event recorded with [`TenantTx::publish`], as returned by [`TenantTx::finish`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordedEvent {
    pub topic: String,
    pub payload: Value,
}

impl TenantTx {
    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Connection to run queries on, e.g. `.fetch_all(tx.conn())`.
    pub fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    /// Records a domain event in the outbox, atomically with this transaction.
    /// It is published only if the transaction commits.
    ///
    /// Topic convention: `<module>.<entity>.<event>`, e.g. `masterdata.partner.created`.
    pub async fn publish(&mut self, topic: &str, payload: &impl Serialize) -> Result<Uuid> {
        let payload = serde_json::to_value(payload)
            .map_err(|e| Error::Validation(format!("event payload: {e}")))?;
        let id = new_id();
        sqlx::query!(
            "INSERT INTO outbox (id, tenant_id, topic, payload, actor) VALUES ($1, $2, $3, $4, $5)",
            id,
            self.tenant_id.0,
            topic,
            payload,
            self.actor,
        )
        .execute(&mut *self.tx)
        .await?;
        self.events.push(RecordedEvent {
            topic: topic.to_string(),
            payload,
        });
        Ok(id)
    }

    /// Opens a savepoint, so that work done after it can be undone with
    /// [`TenantTx::rollback_to`] without giving up the whole transaction.
    ///
    /// Savepoints nest: every savepoint must be released or rolled back
    /// before the one opened before it.
    pub async fn savepoint(&mut self) -> Result<Savepoint> {
        sqlx::query!("SAVEPOINT rostwerk_savepoint")
            .execute(&mut *self.tx)
            .await?;
        Ok(Savepoint {
            events: self.events.len(),
        })
    }

    /// Keeps the work done since `savepoint`.
    pub async fn release(&mut self, _savepoint: Savepoint) -> Result<()> {
        sqlx::query!("RELEASE SAVEPOINT rostwerk_savepoint")
            .execute(&mut *self.tx)
            .await?;
        Ok(())
    }

    /// Undoes the work done since `savepoint`: its rows, audit entries and
    /// outbox events are gone and its events are no longer recorded. The
    /// transaction is usable again, even after a failed statement.
    pub async fn rollback_to(&mut self, savepoint: Savepoint) -> Result<()> {
        // Postgres resolves the name to the most recent savepoint with that name,
        // which is `savepoint` as long as savepoints are used in nested order.
        sqlx::query!("ROLLBACK TO SAVEPOINT rostwerk_savepoint")
            .execute(&mut *self.tx)
            .await?;
        sqlx::query!("RELEASE SAVEPOINT rostwerk_savepoint")
            .execute(&mut *self.tx)
            .await?;
        self.events.truncate(savepoint.events);
        Ok(())
    }

    /// Commits the transaction.
    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await?;
        Ok(())
    }

    /// Commits the transaction, or rolls it back if `dry_run` is set. Either way
    /// returns the events published in it, in order.
    ///
    /// A rolled-back transaction leaves nothing behind: no rows, no audit
    /// entries (written by trigger in the same transaction), no outbox events.
    pub async fn finish(self, dry_run: bool) -> Result<Vec<RecordedEvent>> {
        if dry_run {
            self.tx.rollback().await?;
        } else {
            self.tx.commit().await?;
        }
        Ok(self.events)
    }
}

/// A savepoint in a [`TenantTx`], from [`TenantTx::savepoint`].
#[must_use = "release or roll back a savepoint"]
#[derive(Debug)]
pub struct Savepoint {
    /// Number of events recorded before the savepoint.
    events: usize,
}

/// A cross-tenant transaction for core jobs. See [`Db::begin_system`].
pub struct SystemTx {
    tx: Transaction<'static, Postgres>,
}

impl SystemTx {
    pub fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await?;
        Ok(())
    }
}
