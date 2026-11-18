use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use core_kernel::{Db, Error, TenantCtx};
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use utoipa::openapi::OpenApi;

use crate::bus::{Bus, Entry, OperationInfo, Operations};
use crate::ext::{ExtEntities, ExtEntityInfo, check_entity_name};
use crate::{HostRouter, Module, ModuleManifest};

/// Migrations of the core itself (tenant registry).
fn core_migrator() -> Migrator {
    sqlx::migrate!("./migrations")
}

/// All modules compiled into this binary.
#[derive(Default)]
pub struct ModuleRegistry {
    modules: Vec<Box<dyn Module>>,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a module.
    ///
    /// # Panics
    ///
    /// If a module with the same id is registered already, or one of its
    /// extensible entities is not named `<module id>.<entity>`.
    pub fn register(mut self, module: impl Module) -> Self {
        let id = module.manifest().id;
        assert!(
            self.modules.iter().all(|m| m.manifest().id != id),
            "module {id} registered twice"
        );
        let mut names: Vec<_> = module
            .extensible_entities()
            .iter()
            .map(|e| e.name)
            .collect();
        for name in &names {
            assert!(
                check_entity_name(id, name),
                "extensible entity {name} must be named {id}.<entity> (lower case, digits, _)"
            );
        }
        names.sort_unstable();
        if let Some(dup) = names.windows(2).find(|w| w[0] == w[1]) {
            panic!("extensible entity {} declared twice", dup[0]);
        }
        self.modules.push(Box::new(module));
        self
    }

    pub fn manifests(&self) -> Vec<ModuleManifest> {
        self.modules.iter().map(|m| m.manifest()).collect()
    }

    pub fn is_known(&self, module_id: &str) -> bool {
        self.modules.iter().any(|m| m.manifest().id == module_id)
    }

    /// Runs core migrations, then each module's. Needs a role that may run DDL.
    pub async fn migrate(&self, admin_pool: &PgPool) -> anyhow::Result<()> {
        let migrators = std::iter::once(("core", core_migrator())).chain(
            self.modules
                .iter()
                .filter_map(|m| m.migrator().map(|mig| (m.manifest().id, mig))),
        );
        for (id, mut migrator) in migrators {
            // All migrators share one history table; each ignores the others' entries.
            migrator.set_ignore_missing(true);
            migrator.run(admin_pool).await?;
            tracing::info!(module = id, "migrations applied");
        }
        Ok(())
    }

    /// The extensible entities of all modules.
    pub fn ext_entities(&self) -> ExtEntities {
        ExtEntities::new(self.modules.iter().flat_map(|m| {
            let module = m.manifest().id;
            m.extensible_entities()
                .into_iter()
                .map(move |e| ExtEntityInfo {
                    name: e.name,
                    module,
                    label: e.label,
                })
        }))
    }

    /// The query and command bus over all modules' operations.
    ///
    /// # Panics
    ///
    /// If two modules register the same operation name, or a module registers
    /// one outside its own namespace.
    pub fn bus(&self, db: Db) -> Bus {
        Bus::new(db, self.operation_entries())
    }

    /// All operations of all modules with their schemas, ordered by name.
    /// Input for agent tool specs.
    pub fn operations(&self) -> Vec<OperationInfo> {
        let mut infos: Vec<_> = self
            .operation_entries()
            .into_iter()
            .map(|e| e.info)
            .collect();
        infos.sort_by_key(|i| i.name);
        infos
    }

    fn operation_entries(&self) -> Vec<Entry> {
        self.modules
            .iter()
            .flat_map(|module| {
                let mut ops = Operations::new(module.manifest().id);
                module.operations(&mut ops);
                ops.into_entries()
            })
            .collect()
    }

    /// Routes of all modules, each behind its per-tenant gate, plus their OpenAPI.
    pub fn router(&self) -> (HostRouter, OpenApi) {
        let mut router = HostRouter::new();
        let mut api = OpenApi::default();
        for module in &self.modules {
            let id = module.manifest().id;
            let (routes, module_api) = module.routes().split_for_parts();
            let gated = routes.route_layer(middleware::from_fn(move |req: Request, next: Next| {
                module_gate(id, req, next)
            }));
            router = router.merge(gated);
            api.merge(module_api);
        }
        (router, api)
    }
}

/// Lets a request through only if the tenant has the module enabled.
async fn module_gate(module_id: &'static str, req: Request, next: Next) -> Response {
    match req.extensions().get::<TenantCtx>() {
        None => Error::Unauthorized.into_response(),
        // 404, not 403: a disabled module does not exist for that tenant.
        Some(ctx) if !ctx.has_module(module_id) => Error::NotFound.into_response(),
        Some(_) => next.run(req).await,
    }
}
