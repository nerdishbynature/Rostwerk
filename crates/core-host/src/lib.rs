//! Rostwerk module host.
//!
//! Every module is compiled in and always deployed. Whether a tenant can see a
//! module is decided per request: the gate installed by [`ModuleRegistry`]
//! answers 404 for modules the tenant has not enabled.

pub mod audit;
pub mod bus;
pub mod dry_run;
pub mod members;
pub mod outbox;
mod registry;
pub mod tenants;

use axum::Router;
use core_kernel::Db;
use sqlx::migrate::Migrator;
use utoipa_axum::router::OpenApiRouter;

pub use bus::{Bus, Operation, OperationInfo, OperationKind, OperationSchema, Operations};
pub use dry_run::DryRun;
pub use registry::ModuleRegistry;

/// State shared by all routes.
#[derive(Clone)]
pub struct CoreState {
    pub db: Db,
    /// Queries and commands of all modules.
    pub bus: Bus,
}

/// Static description of a module.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModuleManifest {
    /// Stable id, used in `tenants.modules`, e.g. `masterdata`.
    pub id: &'static str,
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
}

/// A business module (Verkauf, Lager, FiBu, ...).
///
/// Rules: a module never reads another module's tables. It talks to other
/// modules only through queries, commands and events of the core.
pub trait Module: Send + Sync + 'static {
    fn manifest(&self) -> ModuleManifest;

    /// The module's own migrations. Versions must be unique across all modules.
    fn migrator(&self) -> Option<Migrator> {
        None
    }

    /// HTTP routes with their OpenAPI description.
    fn routes(&self) -> OpenApiRouter<CoreState>;

    /// Queries and commands this module offers to other modules via the [`Bus`].
    fn operations(&self, _ops: &mut Operations) {}
}

/// Plain router type that the host hands to the application.
pub type HostRouter = Router<CoreState>;
