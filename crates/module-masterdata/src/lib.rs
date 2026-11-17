//! Master data module: business partners (customers and suppliers) and articles.

mod articles;
mod ops;
mod partners;

use core_host::{CoreState, Module, ModuleManifest, Operations};
use sqlx::migrate::Migrator;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub const MODULE_ID: &str = "masterdata";

pub struct Masterdata;

impl Module for Masterdata {
    fn manifest(&self) -> ModuleManifest {
        ModuleManifest {
            id: MODULE_ID,
            name: "Stammdaten",
            version: env!("CARGO_PKG_VERSION"),
            description: "Business partners (customers and suppliers) and articles",
        }
    }

    fn migrator(&self) -> Option<Migrator> {
        Some(sqlx::migrate!("./migrations"))
    }

    fn routes(&self) -> OpenApiRouter<CoreState> {
        OpenApiRouter::new()
            .routes(routes!(partners::list, partners::create))
            .routes(routes!(partners::get, partners::update))
            .routes(routes!(articles::list, articles::create))
            .routes(routes!(articles::get, articles::update))
    }

    fn operations(&self, ops: &mut Operations) {
        ops::register(ops);
    }
}
