//! Backend-neutral transaction ownership. Adapters join the task's transaction.
use crate::StateError;
use async_trait::async_trait;
use futures::future::BoxFuture;
use std::sync::Arc;

/// REQUIRED propagation: join the outer transaction, otherwise create one.
/// Independent spawned tasks do not inherit transactions. Database workers must
/// capture their adapter's binding before scheduling SQL.
#[crate::extension_point(single, adapter = crate::TransactionManagerPlugin)]
#[async_trait]
pub trait TransactionManager: Send + Sync {
    async fn transaction(
        &self,
        operation: BoxFuture<'_, Result<(), StateError>>,
    ) -> Result<(), StateError>;
}

pub struct TransactionManagerService(pub Arc<dyn TransactionManager>);
pub struct TransactionManagerPlugin(pub Arc<dyn TransactionManager>);
impl TransactionManagerPlugin {
    pub fn from_arc(manager: Arc<dyn TransactionManager>) -> Self {
        Self(manager)
    }
}
#[async_trait]
impl crate::Plugin for TransactionManagerPlugin {
    fn id(&self) -> &str {
        "framework.transaction-manager"
    }
    async fn install(
        &self,
        context: &mut crate::PluginContext<'_>,
    ) -> Result<(), crate::PluginError> {
        context.provide(TransactionManagerService(self.0.clone()))?;
        context.contribute_extension(self.0.clone())
    }
}
