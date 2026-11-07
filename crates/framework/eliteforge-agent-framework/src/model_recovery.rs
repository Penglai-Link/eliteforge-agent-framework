use crate::*;
use async_trait::async_trait;
use std::sync::Arc;

/// Host permission to replace an unavailable chat model within one invocation.
/// Automatic choices may also fail over on quota exhaustion before output;
/// the exhausted model is never retried within that invocation.
/// Empty candidates (the default) means report the original failure, never switch.
#[derive(Clone, Debug, Default)]
pub struct ModelRecoveryPolicy {
    pub candidates: Vec<ModelRef>,
    pub allow_explicit_model_replacement: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelAccountIdentity {
    pub scope: String,
    pub revision: u64,
}

/// A host-shared connection refreshes only its partition. It must reject a
/// result if the account changed while discovery was running.
#[async_trait]
pub trait ModelCatalogRefresh: Send + Sync {
    fn connection_id(&self) -> &str;
    fn account_identity(&self) -> Option<ModelAccountIdentity>;
    /// Nonblocking notification, including failures where replacement is disabled.
    fn request_refresh(&self) {}
    async fn refresh(&self) -> Result<ModelRuntimeUpdate, ModelError>;
}

pub struct ModelCatalogRefreshService(pub Arc<dyn ModelCatalogRefresh>);

pub(crate) struct RecoveryState {
    pub failed: Vec<ModelRef>,
    pub refreshed: bool,
    pub identities: Vec<(Arc<dyn ModelCatalogRefresh>, Option<ModelAccountIdentity>)>,
}

impl RecoveryState {
    pub fn new(refreshers: &[Arc<dyn ModelCatalogRefresh>]) -> Self {
        Self {
            failed: Vec::new(),
            refreshed: false,
            identities: refreshers
                .iter()
                .map(|service| (service.clone(), service.account_identity()))
                .collect(),
        }
    }

    pub fn check_account(&self, model: &ModelRef) -> Result<(), ModelError> {
        if self.identities.iter().any(|(service, identity)| {
            service.connection_id() == model.connection
                && (identity.is_none() || service.account_identity() != *identity)
        }) {
            return Err(ModelError::Provider(Box::new(ProviderModelError {
                authorization_attempt: None,
                provider: model.connection.clone(),
                category: ProviderErrorCategory::AccountChanged,
                http_status: None,
                business_code: None,
                error_type: None,
                model: Some(model.clone()),
                message: "account changed during model invocation".into(),
            })));
        }
        Ok(())
    }
}

/// Installs the host's recovery permissions; this does not change selection
/// during normal successful calls or authorize retrying an entire run.
pub struct ModelRecoveryPlugin(pub ModelRecoveryPolicy);

#[async_trait]
impl Plugin for ModelRecoveryPlugin {
    fn id(&self) -> &str {
        "framework.model-recovery"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(self.0.clone())
    }
}

/// Authorized model runtime for one run. Hosts may share an Agent without
/// publishing one workspace/account's candidates into another run's catalog.
/// This changes bindings and selection only; invocation control remains owned
/// by ModelInvoker, including middleware, retries and shared step reservations.
#[derive(Clone)]
pub struct ModelExecutionScope {
    pub runtime: Arc<ModelRuntime>,
    pub strategy: Arc<dyn ModelSelectionStrategy>,
    pub recovery: ModelRecoveryPolicy,
    pub refreshers: Vec<Arc<dyn ModelCatalogRefresh>>,
}
impl ModelExecutionScope {
    pub fn new(
        runtime: Arc<ModelRuntime>,
        strategy: Arc<dyn ModelSelectionStrategy>,
    ) -> Result<Self, AgentError> {
        strategy.validate(&runtime.snapshot().chat)?;
        Ok(Self {
            runtime,
            strategy,
            recovery: ModelRecoveryPolicy::default(),
            refreshers: vec![],
        })
    }
    pub fn attach(&self, request: &mut RunRequest) {
        request.extensions.insert(self.clone());
    }
}
