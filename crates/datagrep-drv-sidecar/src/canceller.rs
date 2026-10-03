use std::sync::Arc;

use datagrep_api::caps::Caps;
use datagrep_api::driver::{BoxFuture, CancelKind, CancelOutcome, Canceller};
use datagrep_api::error::DbError;

use crate::connection::Link;

pub struct SidecarCanceller {
    link: Arc<Link>,
    kind: CancelKind,
}

impl SidecarCanceller {
    pub(crate) fn new(link: Arc<Link>, flags: Caps) -> Self {
        let kind = if flags.contains(Caps::SERVER_CANCEL) {
            CancelKind::ServerSide
        } else {
            CancelKind::ClientAbandon
        };
        Self { link, kind }
    }
}

impl Canceller for SidecarCanceller {
    fn kind(&self) -> CancelKind {
        self.kind
    }

    fn cancel(&self) -> BoxFuture<'_, Result<CancelOutcome, DbError>> {
        Box::pin(async move {
            self.link.cancel().await?;
            if self.link.settle_or_kill().await {
                Ok(match self.kind {
                    CancelKind::ServerSide => CancelOutcome::Requested,
                    _ => CancelOutcome::ClientAbandoned,
                })
            } else {
                Ok(CancelOutcome::ClientAbandoned)
            }
        })
    }
}
