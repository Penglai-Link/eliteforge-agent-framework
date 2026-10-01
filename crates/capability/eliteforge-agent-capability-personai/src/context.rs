use crab_serverapi::base::AuthorizedClientContext;
use std::sync::Arc;
use tokio::sync::watch;

/// One host account and the credentials current for this revision.
#[derive(Clone)]
pub struct AccountContext {
    pub account_scope: String,
    pub revision: u64,
    pub authorization: AuthorizedClientContext,
}

impl std::fmt::Debug for AccountContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountContext")
            .field("account_scope", &self.account_scope)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

/// The host owns login, token refresh, device binding, and account switching.
pub trait PersonAiContextProvider: Send + Sync {
    fn current(&self) -> Option<Arc<AccountContext>>;
    fn subscribe(&self) -> watch::Receiver<Option<Arc<AccountContext>>>;
    /// Connection closure differs from account changes: even queries using a
    /// retained job binding must stop when its host connection is closed.
    fn is_closed(&self) -> bool {
        false
    }
}

/// A simple host-owned provider. Publish a higher revision after changing credentials.
pub struct PersonAiContextHandle {
    sender: watch::Sender<Option<Arc<AccountContext>>>,
}

impl Default for PersonAiContextHandle {
    fn default() -> Self {
        let (sender, _) = watch::channel(None);
        Self { sender }
    }
}

impl PersonAiContextHandle {
    pub fn publish(&self, context: AccountContext) {
        self.sender.send_replace(Some(Arc::new(context)));
    }

    pub fn logout(&self) {
        self.sender.send_replace(None);
    }
}

impl PersonAiContextProvider for PersonAiContextHandle {
    fn current(&self) -> Option<Arc<AccountContext>> {
        self.sender.borrow().clone()
    }

    fn subscribe(&self) -> watch::Receiver<Option<Arc<AccountContext>>> {
        self.sender.subscribe()
    }
}
