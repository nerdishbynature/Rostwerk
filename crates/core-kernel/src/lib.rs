//! Rostwerk kernel.
//!
//! Everything a module needs to touch data safely lives here: the [`TenantCtx`]
//! that every request carries, the tenant-scoped [`TenantTx`] that is the only
//! way modules reach the database, IDs, [`Money`] and the shared [`Error`].

mod db;
mod error;
mod id;
mod money;
mod tenant;

pub use db::{Db, SystemTx, TenantTx};
pub use error::{Error, Result};
pub use id::{TenantId, new_id};
pub use money::{Currency, Money};
pub use tenant::{Actor, TenantCtx};
