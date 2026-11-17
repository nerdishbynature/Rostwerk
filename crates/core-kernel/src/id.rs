use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// New entity ID. UUIDv7: time-ordered, index friendly, unique across tenants.
pub fn new_id() -> Uuid {
    Uuid::now_v7()
}

/// ID of a tenant (Mandant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[serde(transparent)]
#[sqlx(transparent)]
pub struct TenantId(pub Uuid);

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
