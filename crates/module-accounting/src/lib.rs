//! Accounting module (FiBu): a tenant's chart of accounts and its double-entry
//! journal.
//!
//! Journal entries start as drafts and are posted with a gapless per-tenant
//! sequence number. Posted entries and their lines are immutable (GoBD); the
//! database enforces this with triggers, see the migrations. Mistakes are
//! corrected with a reversal entry.
//!
//! A tenant can set up its chart from the standard charts SKR03 or SKR04
//! (`charts/`). Lines carry optional Rostwerk tax codes, independent of DATEV.

mod accounts;
mod charts;
mod common;
mod entries;
mod tax;

use core_host::{CoreState, Module, ModuleManifest};
use sqlx::migrate::Migrator;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub const MODULE_ID: &str = "accounting";

pub struct Accounting;

impl Module for Accounting {
    fn manifest(&self) -> ModuleManifest {
        ModuleManifest {
            id: MODULE_ID,
            name: "Finanzbuchhaltung",
            version: env!("CARGO_PKG_VERSION"),
            description: "Chart of accounts and immutable double-entry journal",
        }
    }

    fn migrator(&self) -> Option<Migrator> {
        Some(sqlx::migrate!("./migrations"))
    }

    fn routes(&self) -> OpenApiRouter<CoreState> {
        OpenApiRouter::new()
            .routes(routes!(accounts::list, accounts::create))
            .routes(routes!(accounts::get, accounts::update, accounts::delete))
            .routes(routes!(accounts::balance))
            .routes(routes!(charts::get))
            .routes(routes!(charts::setup))
            .routes(routes!(tax::list))
            .routes(routes!(entries::list, entries::create))
            .routes(routes!(entries::get, entries::update, entries::delete))
            .routes(routes!(entries::post))
            .routes(routes!(entries::reverse))
    }
}
