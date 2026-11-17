use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use core_kernel::{Error, TenantCtx};
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use utoipa::openapi::OpenApi;

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

    pub fn register(mut self, module: impl Module) -> Self {
        let id = module.manifest().id;
        assert!(
            self.modules.iter().all(|m| m.manifest().id != id),
            "module {id} registered twice"
        );
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
