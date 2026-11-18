//! In-process query and command bus between modules.
//!
//! A module never reads another module's tables. Instead it registers named
//! operations (see [`Module::operations`](crate::Module::operations)) and other
//! modules call them by name through the [`Bus`]:
//!
//! ```ignore
//! let partner: PartnerView = bus.query(&ctx, "masterdata.partner.get", &GetPartner { id }).await?;
//! ```
//!
//! Requests and responses cross the bus as JSON, so a caller does not depend
//! on the callee's crate: it brings its own serde types for the fields it
//! needs. The same JSON form, with the JSON Schemas from
//! [`Bus::operations`], is what agent tool specs are generated from.
//!
//! Every call runs in the caller's [`TenantCtx`], so RLS is armed for the
//! caller's tenant. Queries run read-only; commands may change data and
//! [`TenantTx::publish`] events.
//!
//! # Transactions
//!
//! [`Bus::query`], [`Bus::command`] and [`Bus::call_json`] open their own
//! [`TenantTx`] and commit it when the handler succeeds. Use them where there
//! is no transaction yet, e.g. for an agent tool call.
//!
//! A write handler that already holds a [`TenantTx`] calls other modules'
//! commands with [`Bus::command_in`] instead. The command then runs in the
//! caller's transaction, so the caller and the command commit or roll back
//! together:
//!
//! - If the caller fails after the command succeeded, the command's changes
//!   and events are rolled back too.
//! - In a dry run ([`DryRun`](crate::DryRun)) nothing of the command is
//!   persisted, and its events appear in the dry-run response.
//! - If the command itself fails, only its own work is undone (it runs in a
//!   savepoint); the caller gets the error and may handle it and go on.
//!
//! ```ignore
//! let mut tx = ctx.begin(&state.db).await?;
//! let order = /* insert the order */;
//! let reserved: Reserved = state.bus.command_in(&ctx, &mut tx, "stock.item.reserve", &req).await?;
//! dry_run.respond(tx, StatusCode::CREATED, order).await
//! ```
//!
//! [`Bus::command`] commits on its own: calling it from a write handler would
//! persist the command even if the handler fails or is a dry run.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use core_kernel::{Db, Error, Result, TenantCtx, TenantTx};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use utoipa::ToSchema;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::Schema;

/// Whether an operation only reads (query) or may change data (command).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationKind {
    Query,
    Command,
}

impl fmt::Display for OperationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OperationKind::Query => "query",
            OperationKind::Command => "command",
        })
    }
}

/// A named query or command that a module offers to other modules.
///
/// Implement it on a unit struct and register it in
/// [`Module::operations`](crate::Module::operations):
///
/// ```ignore
/// struct GetPartner;
///
/// impl Operation for GetPartner {
///     const NAME: &'static str = "masterdata.partner.get";
///     const KIND: OperationKind = OperationKind::Query;
///     const DESCRIPTION: &'static str = "Get one business partner by id.";
///     type Request = PartnerRef;
///     type Response = Partner;
///
///     async fn run(&self, tx: &mut TenantTx, req: PartnerRef) -> Result<Partner> { ... }
/// }
/// ```
pub trait Operation: Send + Sync + 'static {
    /// `<module>.<entity>.<verb>`, e.g. `masterdata.partner.get`. Must start
    /// with the registering module's id.
    const NAME: &'static str;
    const KIND: OperationKind;
    /// One or two sentences, shown to agents as the tool description.
    const DESCRIPTION: &'static str;

    type Request: DeserializeOwned + ToSchema + Send;
    type Response: Serialize + ToSchema;

    /// Runs the operation in a tenant transaction. If this returns an error,
    /// everything it did in `tx` is rolled back. See [the module docs](self)
    /// for when the transaction is committed.
    fn run(
        &self,
        tx: &mut TenantTx,
        req: Self::Request,
    ) -> impl Future<Output = Result<Self::Response>> + Send;
}

/// A JSON Schema with the named schemas it refers to.
///
/// References have the OpenAPI form `#/components/schemas/<Name>`; every name
/// referred to (directly or transitively) is in `components`.
#[derive(Clone, Serialize)]
pub struct OperationSchema {
    pub schema: RefOr<Schema>,
    pub components: BTreeMap<String, RefOr<Schema>>,
}

impl OperationSchema {
    fn of<T: ToSchema>() -> Self {
        let mut components = Vec::new();
        T::schemas(&mut components);
        Self {
            schema: T::schema(),
            components: components.into_iter().collect(),
        }
    }
}

/// Description of a registered operation, e.g. for MCP tool specs.
#[derive(Clone, Serialize)]
pub struct OperationInfo {
    pub name: &'static str,
    /// Id of the module that offers it.
    pub module: &'static str,
    pub kind: OperationKind,
    pub description: &'static str,
    pub request: OperationSchema,
    pub response: OperationSchema,
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Object-safe form of [`Operation`], speaking JSON.
trait ErasedOperation: Send + Sync {
    fn call<'a>(&'a self, tx: &'a mut TenantTx, req: Value) -> BoxFuture<'a, Result<Value>>;
}

struct Erased<O>(O);

impl<O: Operation> ErasedOperation for Erased<O> {
    fn call<'a>(&'a self, tx: &'a mut TenantTx, req: Value) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let req = serde_json::from_value(req)
                .map_err(|e| Error::Validation(format!("{} request: {e}", O::NAME)))?;
            let resp = self.0.run(tx, req).await?;
            serde_json::to_value(resp)
                .map_err(|e| Error::Validation(format!("{} response: {e}", O::NAME)))
        })
    }
}

pub(crate) struct Entry {
    pub(crate) info: OperationInfo,
    handler: Box<dyn ErasedOperation>,
}

/// Collects the operations of one module. Handed to
/// [`Module::operations`](crate::Module::operations).
pub struct Operations {
    module: &'static str,
    entries: Vec<Entry>,
}

impl Operations {
    pub(crate) fn new(module: &'static str) -> Self {
        Self {
            module,
            entries: Vec::new(),
        }
    }

    /// Registers an operation.
    ///
    /// # Panics
    ///
    /// If its name does not start with `<module id>.`.
    pub fn add<O: Operation>(&mut self, op: O) -> &mut Self {
        let prefix = O::NAME.split_once('.').map(|(p, _)| p);
        assert!(
            prefix == Some(self.module),
            "operation {} must be named {}.<entity>.<verb>",
            O::NAME,
            self.module
        );
        self.entries.push(Entry {
            info: OperationInfo {
                name: O::NAME,
                module: self.module,
                kind: O::KIND,
                description: O::DESCRIPTION,
                request: OperationSchema::of::<O::Request>(),
                response: OperationSchema::of::<O::Response>(),
            },
            handler: Box::new(Erased(op)),
        });
        self
    }

    pub(crate) fn into_entries(self) -> Vec<Entry> {
        self.entries
    }
}

/// Dispatches queries and commands to the module that registered them.
///
/// Built by [`ModuleRegistry::bus`](crate::ModuleRegistry::bus). Cheap to clone.
#[derive(Clone)]
pub struct Bus {
    db: Db,
    ops: Arc<HashMap<&'static str, Entry>>,
}

impl Bus {
    pub(crate) fn new(db: Db, entries: impl IntoIterator<Item = Entry>) -> Self {
        let mut ops = HashMap::new();
        for entry in entries {
            let name = entry.info.name;
            assert!(
                ops.insert(name, entry).is_none(),
                "operation {name} registered twice"
            );
        }
        Self {
            db,
            ops: Arc::new(ops),
        }
    }

    /// All registered operations with their schemas, ordered by name.
    pub fn operations(&self) -> Vec<OperationInfo> {
        let mut infos: Vec<_> = self.ops.values().map(|e| e.info.clone()).collect();
        infos.sort_by_key(|i| i.name);
        infos
    }

    /// Runs a query in its own read-only transaction. Fails with [`Error::NotFound`] if no query of that name
    /// exists or the tenant has not enabled its module.
    pub async fn query<Req, Resp>(&self, ctx: &TenantCtx, name: &str, req: &Req) -> Result<Resp>
    where
        Req: Serialize + ?Sized,
        Resp: DeserializeOwned,
    {
        let req = to_request(name, req)?;
        let resp = self
            .dispatch(ctx, Some(OperationKind::Query), name, req)
            .await?;
        from_response(name, resp)
    }

    /// Runs a command in its own transaction and commits it. Fails with
    /// [`Error::NotFound`] if no command of that name exists or the tenant has
    /// not enabled its module.
    ///
    /// From a handler that holds a [`TenantTx`], use [`Bus::command_in`].
    pub async fn command<Req, Resp>(&self, ctx: &TenantCtx, name: &str, req: &Req) -> Result<Resp>
    where
        Req: Serialize + ?Sized,
        Resp: DeserializeOwned,
    {
        let req = to_request(name, req)?;
        let resp = self
            .dispatch(ctx, Some(OperationKind::Command), name, req)
            .await?;
        from_response(name, resp)
    }

    /// Runs a command in the caller's transaction `tx`, so it commits or rolls
    /// back together with the caller's own changes (see [the module docs](self)).
    /// If the command fails, its work is undone and `tx` stays usable.
    ///
    /// Fails with [`Error::NotFound`] like [`Bus::command`], and with
    /// [`Error::Forbidden`] if `tx` belongs to another tenant than `ctx`.
    pub async fn command_in<Req, Resp>(
        &self,
        ctx: &TenantCtx,
        tx: &mut TenantTx,
        name: &str,
        req: &Req,
    ) -> Result<Resp>
    where
        Req: Serialize + ?Sized,
        Resp: DeserializeOwned,
    {
        if tx.tenant_id() != ctx.tenant_id() {
            return Err(Error::Forbidden);
        }
        let entry = self.entry(ctx, Some(OperationKind::Command), name)?;
        let req = to_request(name, req)?;

        let savepoint = tx.savepoint().await?;
        match entry.handler.call(tx, req).await {
            Ok(resp) => {
                tx.release(savepoint).await?;
                from_response(name, resp)
            }
            Err(err) => {
                tx.rollback_to(savepoint).await?;
                Err(err)
            }
        }
    }

    /// Runs a query or command with a JSON request, e.g. from an agent tool
    /// call, in its own transaction.
    pub async fn call_json(&self, ctx: &TenantCtx, name: &str, req: Value) -> Result<Value> {
        self.dispatch(ctx, None, name, req).await
    }

    /// The operation `name` of `kind` (any if `None`), if the tenant may use it.
    fn entry(&self, ctx: &TenantCtx, kind: Option<OperationKind>, name: &str) -> Result<&Entry> {
        self.ops
            .get(name)
            .filter(|e| kind.is_none_or(|k| e.info.kind == k))
            // Same rule as the HTTP gate: a disabled module does not exist for the tenant.
            .filter(|e| ctx.has_module(e.info.module))
            .ok_or(Error::NotFound)
    }

    /// Runs an operation in a new transaction and commits it on success.
    async fn dispatch(
        &self,
        ctx: &TenantCtx,
        kind: Option<OperationKind>,
        name: &str,
        req: Value,
    ) -> Result<Value> {
        let entry = self.entry(ctx, kind, name)?;
        let mut tx = ctx.begin(&self.db).await?;
        if entry.info.kind == OperationKind::Query {
            sqlx::query!("SET TRANSACTION READ ONLY")
                .execute(tx.conn())
                .await?;
        }
        // On error `tx` is dropped, which rolls it back.
        let resp = entry.handler.call(&mut tx, req).await?;
        tx.commit().await?;
        Ok(resp)
    }
}

fn to_request<Req: Serialize + ?Sized>(name: &str, req: &Req) -> Result<Value> {
    serde_json::to_value(req).map_err(|e| Error::Validation(format!("{name} request: {e}")))
}

fn from_response<Resp: DeserializeOwned>(name: &str, resp: Value) -> Result<Resp> {
    serde_json::from_value(resp).map_err(|e| Error::Validation(format!("{name} response: {e}")))
}
