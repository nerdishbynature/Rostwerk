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
//! Every call runs in the caller's [`TenantCtx`]: the bus opens a
//! [`TenantTx`] for the caller's tenant (RLS is armed), runs the handler in it
//! and commits on success. Queries get a read-only transaction; commands get a
//! read-write one, so they can change data and [`TenantTx::publish`] events
//! atomically. A failed handler rolls everything back.

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

    /// Runs the operation in the caller's tenant transaction. The bus commits
    /// it if this returns `Ok`, and rolls it back otherwise.
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

    /// Runs a query. Fails with [`Error::NotFound`] if no query of that name
    /// exists or the tenant has not enabled its module.
    pub async fn query<Req, Resp>(&self, ctx: &TenantCtx, name: &str, req: &Req) -> Result<Resp>
    where
        Req: Serialize + ?Sized,
        Resp: DeserializeOwned,
    {
        self.typed(ctx, Some(OperationKind::Query), name, req).await
    }

    /// Runs a command. Fails with [`Error::NotFound`] if no command of that
    /// name exists or the tenant has not enabled its module.
    pub async fn command<Req, Resp>(&self, ctx: &TenantCtx, name: &str, req: &Req) -> Result<Resp>
    where
        Req: Serialize + ?Sized,
        Resp: DeserializeOwned,
    {
        self.typed(ctx, Some(OperationKind::Command), name, req)
            .await
    }

    /// Runs a query or command with a JSON request, e.g. from an agent tool call.
    pub async fn call_json(&self, ctx: &TenantCtx, name: &str, req: Value) -> Result<Value> {
        self.dispatch(ctx, None, name, req).await
    }

    async fn typed<Req, Resp>(
        &self,
        ctx: &TenantCtx,
        kind: Option<OperationKind>,
        name: &str,
        req: &Req,
    ) -> Result<Resp>
    where
        Req: Serialize + ?Sized,
        Resp: DeserializeOwned,
    {
        let req = serde_json::to_value(req)
            .map_err(|e| Error::Validation(format!("{name} request: {e}")))?;
        let resp = self.dispatch(ctx, kind, name, req).await?;
        serde_json::from_value(resp).map_err(|e| Error::Validation(format!("{name} response: {e}")))
    }

    async fn dispatch(
        &self,
        ctx: &TenantCtx,
        kind: Option<OperationKind>,
        name: &str,
        req: Value,
    ) -> Result<Value> {
        let entry = self
            .ops
            .get(name)
            .filter(|e| kind.is_none_or(|k| e.info.kind == k))
            // Same rule as the HTTP gate: a disabled module does not exist for the tenant.
            .filter(|e| ctx.has_module(e.info.module))
            .ok_or(Error::NotFound)?;

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
