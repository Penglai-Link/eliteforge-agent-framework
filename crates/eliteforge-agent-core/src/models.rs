//! Host-neutral model connections and replaceable selection policies.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModelRef {
    pub connection: String,
    pub model: String,
}
impl ModelRef {
    pub fn new(connection: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            connection: connection.into(),
            model: model.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelMessageContext {
    pub model: ModelRef,
    pub protocol: String,
    pub format: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ModelDescriptor {
    pub id: String,
    /// Total input + output capacity. None leaves the configured input limit authoritative.
    pub context_window_tokens: Option<usize>,
    /// Host-configured input budget after reserving output capacity. None is unknown.
    pub max_input_tokens: Option<usize>,
    /// None means unknown, not unsupported.
    pub supports_tools: Option<bool>,
    pub input_modalities: Vec<Modality>,
    pub output_modalities: Vec<Modality>,
    pub supports_structured_output: Option<bool>,
    /// Namespaced provider metadata for custom selection policies.
    pub metadata: BTreeMap<String, serde_json::Value>,
}
impl ModelDescriptor {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            context_window_tokens: None,
            max_input_tokens: None,
            supports_tools: None,
            input_modalities: vec![Modality::Text],
            output_modalities: vec![Modality::Text],
            supports_structured_output: None,
            metadata: BTreeMap::new(),
        }
    }
    pub fn with_input_modalities(mut self, modalities: impl IntoIterator<Item = Modality>) -> Self {
        self.input_modalities = modalities.into_iter().collect();
        self
    }
    pub fn with_output_modalities(
        mut self,
        modalities: impl IntoIterator<Item = Modality>,
    ) -> Self {
        self.output_modalities = modalities.into_iter().collect();
        self
    }
    pub fn accepts(
        &self,
        messages: &[Message],
        tools: &[ToolDescriptor],
        options: &GenerationOptions,
    ) -> bool {
        messages
            .iter()
            .flat_map(|m| m.content.modalities())
            .all(|m| self.input_modalities.contains(&m))
            && (tools.is_empty() || self.supports_tools != Some(false))
            && (options.output_schema.is_none() || self.supports_structured_output != Some(false))
    }
    pub fn input_capacity(
        &self,
        limits: &RunLimits,
        options: &GenerationOptions,
    ) -> Result<usize, AgentError> {
        let output = options
            .max_output_tokens
            .unwrap_or(limits.max_model_output_tokens) as usize;
        let window = match self.context_window_tokens {
            Some(window) => window.checked_sub(output).ok_or_else(|| {
                AgentError::InvalidConfiguration(
                    "output reserve exceeds model context window".into(),
                )
            })?,
            None => usize::MAX,
        };
        // max_input_tokens already excludes output. Never subtract from it again.
        let capacity = window
            .min(self.max_input_tokens.unwrap_or(limits.max_context_tokens))
            .min(limits.max_context_tokens);
        if capacity == 0 {
            return Err(AgentError::InvalidConfiguration(
                "model has no input capacity after output reservation".into(),
            ));
        }
        Ok(capacity)
    }
}

/// A configured protocol adapter. One instance serves many model IDs.
#[crate::extension_point(adapter = crate::ChatModelPlugin)]
#[async_trait]
pub trait ChatModel: Send + Sync {
    async fn generate(
        &self,
        model_id: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError>;
}

/// Optional discovery capability. Hosts filter results before registration.
#[crate::extension_point]
#[async_trait]
pub trait ModelDiscovery: Send + Sync {
    async fn discover(&self) -> Result<Vec<ModelDescriptor>, ModelError>;
}

#[derive(Clone)]
pub struct ChatBinding {
    pub adapter: Arc<dyn ChatModel>,
    pub protocol: String,
}
#[derive(Clone)]
pub struct ModelConnection {
    pub id: String,
    pub models: Vec<ModelDescriptor>,
    pub adapter: Arc<dyn ChatModel>,
    pub protocol: String,
    /// Optional per-model bindings sharing this connection's identity.
    pub bindings: BTreeMap<String, ChatBinding>,
    pub token_estimator: Arc<dyn TokenEstimator>,
}
impl ModelConnection {
    pub fn new(
        id: impl Into<String>,
        provider: Arc<dyn ChatModel>,
        models: Vec<ModelDescriptor>,
    ) -> Self {
        Self {
            id: id.into(),
            adapter: provider,
            protocol: "custom".into(),
            bindings: BTreeMap::new(),
            models,
            token_estimator: Arc::new(ConservativeTokenEstimator),
        }
    }
    pub fn with_protocol(mut self, protocol: impl Into<String>) -> Self {
        self.protocol = protocol.into();
        self
    }
    pub fn bind(
        mut self,
        model: impl Into<String>,
        adapter: Arc<dyn ChatModel>,
        protocol: impl Into<String>,
    ) -> Result<Self, AgentError> {
        if self
            .bindings
            .insert(
                model.into(),
                ChatBinding {
                    adapter,
                    protocol: protocol.into(),
                },
            )
            .is_some()
        {
            return Err(AgentError::InvalidConfiguration(
                "duplicate chat model binding".into(),
            ));
        }
        Ok(self)
    }
    pub fn with_token_estimator(mut self, estimator: Arc<dyn TokenEstimator>) -> Self {
        self.token_estimator = estimator;
        self
    }
}

#[derive(Default)]
pub struct ModelCatalog {
    #[doc(hidden)]
    pub connections: BTreeMap<String, ModelConnection>,
}
impl ModelCatalog {
    pub fn new(connections: Vec<ModelConnection>) -> Result<Self, AgentError> {
        let mut result = Self::default();
        for connection in connections {
            if connection.id.trim().is_empty()
                || connection.models.is_empty()
                || connection.protocol.trim().is_empty()
                || result.connections.contains_key(&connection.id)
            {
                return Err(AgentError::InvalidConfiguration(
                    "empty or duplicate model connection ID".into(),
                ));
            }
            let mut ids = std::collections::HashSet::new();
            for model in &connection.models {
                if model.id.trim().is_empty()
                    || model.input_modalities.is_empty()
                    || model.output_modalities.is_empty()
                    || !ids.insert(&model.id)
                    || model.max_input_tokens == Some(0)
                    || model.context_window_tokens == Some(0)
                {
                    return Err(AgentError::InvalidConfiguration(
                        "invalid or duplicate model descriptor".into(),
                    ));
                }
            }
            if connection
                .bindings
                .iter()
                .any(|(id, b)| b.protocol.trim().is_empty() || !ids.contains(id))
            {
                return Err(AgentError::InvalidConfiguration(
                    "chat binding must refer to a registered model and protocol".into(),
                ));
            }
            result.connections.insert(connection.id.clone(), connection);
        }
        Ok(result)
    }
    pub fn models(&self) -> Vec<ModelRef> {
        self.connections
            .values()
            .flat_map(|c| c.models.iter().map(|m| ModelRef::new(&c.id, &m.id)))
            .collect()
    }
    pub fn get(&self, model: &ModelRef) -> Option<&ModelDescriptor> {
        self.connections
            .get(&model.connection)?
            .models
            .iter()
            .find(|m| m.id == model.model)
    }
    pub fn token_estimator(&self, model: &ModelRef) -> Option<&dyn TokenEstimator> {
        self.get(model)?;
        Some(self.connections[&model.connection].token_estimator.as_ref())
    }
    pub fn adapter(&self, model: &ModelRef) -> Option<&dyn ChatModel> {
        self.get(model)?;
        let connection = &self.connections[&model.connection];
        Some(
            connection
                .bindings
                .get(&model.model)
                .map_or(connection.adapter.as_ref(), |b| b.adapter.as_ref()),
        )
    }
    pub fn protocol(&self, model: &ModelRef) -> Option<&str> {
        self.get(model)?;
        let connection = &self.connections[&model.connection];
        Some(
            connection
                .bindings
                .get(&model.model)
                .map_or(connection.protocol.as_str(), |b| b.protocol.as_str()),
        )
    }
}

pub struct ModelSelectionContext<'a> {
    pub stage: &'a str,
    pub step: usize,
    pub attempt: usize,
    /// Counts transient failures only; context recovery does not advance routing.
    pub transient_failures: usize,
    pub previous: Option<&'a ModelRef>,
    pub tools: &'a [ToolDescriptor],
    /// Current history projection before model-specific reduction. This allows
    /// routing on content/modality, not only the names of stages and tools.
    pub messages: &'a [Message],
    pub options: &'a GenerationOptions,
    pub run: &'a RunContext,
}

#[crate::extension_point(single, adapter = crate::ModelStrategyPlugin)]
#[async_trait]
pub trait ModelSelectionStrategy: Send + Sync {
    fn validate(&self, _catalog: &ModelCatalog) -> Result<(), AgentError> {
        Ok(())
    }
    async fn select(
        &self,
        context: &ModelSelectionContext<'_>,
        catalog: &ModelCatalog,
    ) -> Result<ModelRef, AgentError>;
}

pub struct FixedModelStrategy(pub ModelRef);

#[derive(Default)]
pub struct CatalogModelStrategy(pub ModelRouting);
#[async_trait]
impl ModelSelectionStrategy for CatalogModelStrategy {
    fn validate(&self, catalog: &ModelCatalog) -> Result<(), AgentError> {
        for model in self
            .0
            .defaults
            .iter()
            .filter(|(o, _)| **o == ModelOperation::Chat)
            .map(|(_, m)| m)
            .chain(
                self.0
                    .stages
                    .iter()
                    .filter(|((o, _), _)| *o == ModelOperation::Chat)
                    .map(|(_, m)| m),
            )
        {
            require_model(catalog, model)?;
        }
        Ok(())
    }
    async fn select(
        &self,
        context: &ModelSelectionContext<'_>,
        catalog: &ModelCatalog,
    ) -> Result<ModelRef, AgentError> {
        if let Some(model) = self.0.selected(ModelOperation::Chat, context.stage) {
            return Ok(model.clone());
        }
        let candidates: Vec<_> = catalog
            .models()
            .into_iter()
            .filter(|model| {
                catalog
                    .get(model)
                    .is_some_and(|d| d.accepts(context.messages, context.tools, context.options))
            })
            .collect();
        match candidates.as_slice() {
            [model] => Ok(model.clone()),
            [] => Err(AgentError::InvalidConfiguration(
                "no compatible chat model".into(),
            )),
            _ => Err(AgentError::InvalidConfiguration(
                "multiple compatible chat models require an explicit choice, stage rule or default"
                    .into(),
            )),
        }
    }
}
#[async_trait]
impl ModelSelectionStrategy for FixedModelStrategy {
    fn validate(&self, catalog: &ModelCatalog) -> Result<(), AgentError> {
        require_model(catalog, &self.0)
    }
    async fn select(
        &self,
        _: &ModelSelectionContext<'_>,
        _: &ModelCatalog,
    ) -> Result<ModelRef, AgentError> {
        Ok(self.0.clone())
    }
}

/// Ordered failover. Retries are owned by ModelInvoker, never by this strategy.
pub struct RoutingStrategy {
    pub candidates: Vec<ModelRef>,
}
#[async_trait]
impl ModelSelectionStrategy for RoutingStrategy {
    fn validate(&self, catalog: &ModelCatalog) -> Result<(), AgentError> {
        if self.candidates.is_empty() {
            return Err(AgentError::InvalidConfiguration(
                "routing requires candidates".into(),
            ));
        }
        for model in &self.candidates {
            require_model(catalog, model)?;
        }
        Ok(())
    }
    async fn select(
        &self,
        context: &ModelSelectionContext<'_>,
        _: &ModelCatalog,
    ) -> Result<ModelRef, AgentError> {
        self.candidates
            .get(
                context
                    .transient_failures
                    .min(self.candidates.len().saturating_sub(1)),
            )
            .cloned()
            .ok_or_else(|| AgentError::InvalidConfiguration("routing requires candidates".into()))
    }
}

#[derive(Default)]
pub struct StageModelStrategy {
    pub stages: BTreeMap<String, Arc<dyn ModelSelectionStrategy>>,
    pub fallback: Option<Arc<dyn ModelSelectionStrategy>>,
}
#[async_trait]
impl ModelSelectionStrategy for StageModelStrategy {
    fn validate(&self, catalog: &ModelCatalog) -> Result<(), AgentError> {
        for strategy in self.stages.values().chain(self.fallback.iter()) {
            strategy.validate(catalog)?;
        }
        Ok(())
    }
    async fn select(
        &self,
        context: &ModelSelectionContext<'_>,
        catalog: &ModelCatalog,
    ) -> Result<ModelRef, AgentError> {
        self.stages
            .get(context.stage)
            .or(self.fallback.as_ref())
            .ok_or_else(|| {
                AgentError::InvalidConfiguration(format!(
                    "no model strategy for stage {}",
                    context.stage
                ))
            })?
            .select(context, catalog)
            .await
    }
}

#[doc(hidden)]
pub fn require_model(catalog: &ModelCatalog, model: &ModelRef) -> Result<(), AgentError> {
    catalog.get(model).map(|_| ()).ok_or_else(|| {
        AgentError::InvalidConfiguration(format!(
            "model not in authorized catalog: {}/{}",
            model.connection, model.model
        ))
    })
}

pub struct ModelConnectionService(pub ModelConnection);
pub struct ModelStrategyService(pub Arc<dyn ModelSelectionStrategy>);

pub struct ModelConnectionPlugin {
    id: String,
    connection: ModelConnection,
}
impl ModelConnectionPlugin {
    pub fn new(connection: ModelConnection) -> Self {
        Self {
            id: format!("model.connection.{}", connection.id),
            connection,
        }
    }
}
#[async_trait]
impl Plugin for ModelConnectionPlugin {
    fn id(&self) -> &str {
        &self.id
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.contribute(ModelConnectionService(self.connection.clone()))?;
        context.contribute_extension(self.connection.adapter.clone())
    }
}

pub struct ModelStrategyPlugin(pub Arc<dyn ModelSelectionStrategy>);
impl ModelStrategyPlugin {
    pub fn new(strategy: impl ModelSelectionStrategy + 'static) -> Self {
        Self(Arc::new(strategy))
    }

    pub fn from_arc(strategy: Arc<dyn ModelSelectionStrategy>) -> Self {
        Self(strategy)
    }
}
#[async_trait]
impl Plugin for ModelStrategyPlugin {
    fn id(&self) -> &str {
        "model.strategy"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(ModelStrategyService(self.0.clone()))?;
        context.contribute_extension(self.0.clone())
    }
}

/// Request for one model step. Custom AgentLoops reuse the same control path as ReAct.
pub struct ModelCall<'a> {
    pub stage: &'a str,
    pub step: usize,
    pub instructions: &'a [Message],
    pub tools: &'a [ToolDescriptor],
    pub limits: &'a RunLimits,
    pub options: &'a GenerationOptions,
}

#[async_trait]
pub trait ChatExecutor: Send + Sync {
    fn catalog(&self) -> &ModelCatalog;
    async fn invoke(
        &self,
        call: ModelCall<'_>,
        memory: &mut dyn ConversationSession,
        context: &RunContext,
    ) -> Result<ChatResponse, AgentError>;
    async fn invoke_auxiliary(
        &self,
        model: &ModelRef,
        request: ChatRequest,
        stage: &str,
        limits: &RunLimits,
        context: &RunContext,
    ) -> Result<ChatResponse, AgentError>;
}
