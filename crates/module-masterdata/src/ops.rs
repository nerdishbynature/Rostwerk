//! Queries and commands that other modules call through the bus.

use core_host::{Operation, OperationKind, Operations};
use core_kernel::{Error, Result, TenantTx};
use serde::Deserialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::partners::{self, Partner};

pub(crate) fn register(ops: &mut Operations) {
    ops.add(GetPartner);
}

/// Request of `masterdata.partner.get`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PartnerRef {
    /// Partner id.
    pub id: Uuid,
}

/// `masterdata.partner.get`: one partner of the caller's tenant.
pub struct GetPartner;

impl Operation for GetPartner {
    const NAME: &'static str = "masterdata.partner.get";
    const KIND: OperationKind = OperationKind::Query;
    const DESCRIPTION: &'static str = "Get one business partner (customer or supplier) by id. Fails with not found if it does not exist.";
    type Request = PartnerRef;
    type Response = Partner;

    async fn run(&self, tx: &mut TenantTx, req: PartnerRef) -> Result<Partner> {
        partners::find(tx, req.id).await?.ok_or(Error::NotFound)
    }
}
