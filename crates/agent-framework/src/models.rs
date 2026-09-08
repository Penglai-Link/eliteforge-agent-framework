//! Host-neutral model connections and replaceable selection policies.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

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
    pub supports_media: Option<bool>,
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
            supports_media: None,
            supports_structured_output: None,
            metadata: BTreeMap::new(),
        }
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
#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn generate(
        &self,
        model_id: &str,
        request: ModelRequest,
        stream: &dyn ModelStream,
    ) -> Result<ModelResponse, ModelError>;
}

/// Optional discovery capability. Hosts filter results before registration.
#[async_trait]
pub trait ModelDiscovery: Send + Sync {
    async fn discover(&self) -> Result<Vec<ModelDescriptor>, ModelError>;
}

#[derive(Clone)]
pub struct ModelConnection {
    pub id: String,
    pub models: Vec<ModelDescriptor>,
    pub provider: Arc<dyn ModelProvider>,
    pub token_estimator: Arc<dyn TokenEstimator>,
}
impl ModelConnection {
    pub fn new(
        id: impl Into<String>,
        provider: Arc<dyn ModelProvider>,
        models: Vec<ModelDescriptor>,
    ) -> Self {
        Self {
            id: id.into(),
            provider,
            models,
            token_estimator: Arc::new(ConservativeTokenEstimator),
        }
    }
    pub fn with_token_estimator(mut self, estimator: Arc<dyn TokenEstimator>) -> Self {
        self.token_estimator = estimator;
        self
    }
}

#[derive(Default)]
pub struct ModelCatalog {
    connections: BTreeMap<String, ModelConnection>,
}
impl ModelCatalog {
    pub fn new(connections: Vec<ModelConnection>) -> Result<Self, AgentError> {
        let mut result = Self::default();
        for connection in connections {
            if connection.id.trim().is_empty() || result.connections.contains_key(&connection.id) {
                return Err(AgentError::InvalidConfiguration(
                    "empty or duplicate model connection ID".into(),
                ));
            }
            let mut ids = std::collections::HashSet::new();
            for model in &connection.models {
                if model.id.trim().is_empty()
                    || !ids.insert(&model.id)
                    || model.max_input_tokens == Some(0)
                    || model.context_window_tokens == Some(0)
                {
                    return Err(AgentError::InvalidConfiguration(
                        "invalid or duplicate model descriptor".into(),
                    ));
                }
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

fn require_model(catalog: &ModelCatalog, model: &ModelRef) -> Result<(), AgentError> {
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
        context.contribute(ModelConnectionService(self.connection.clone()))
    }
}

pub struct ModelStrategyPlugin(pub Arc<dyn ModelSelectionStrategy>);
#[async_trait]
impl Plugin for ModelStrategyPlugin {
    fn id(&self) -> &str {
        "model.strategy"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(ModelStrategyService(self.0.clone()))
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

pub struct ModelInvoker {
    catalog: ModelCatalog,
    strategy: Arc<dyn ModelSelectionStrategy>,
    middleware: Vec<Arc<dyn ModelMiddleware>>,
    assembler: Arc<dyn ContextAssembler>,
}
impl ModelInvoker {
    pub fn new(
        catalog: ModelCatalog,
        strategy: Option<Arc<dyn ModelSelectionStrategy>>,
    ) -> Result<Self, AgentError> {
        let strategy = match strategy {
            Some(strategy) => strategy,
            None => {
                let models = catalog.models();
                if models.len() != 1 {
                    return Err(AgentError::InvalidConfiguration(
                        "multiple models require an explicit selection strategy".into(),
                    ));
                }
                Arc::new(FixedModelStrategy(models[0].clone()))
            }
        };
        strategy.validate(&catalog)?;
        Ok(Self {
            catalog,
            strategy,
            middleware: Vec::new(),
            assembler: Arc::new(DefaultContextAssembler::default()),
        })
    }
    pub fn with_middleware(mut self, middleware: Vec<Arc<dyn ModelMiddleware>>) -> Self {
        self.middleware = middleware;
        self
    }
    pub fn with_assembler(mut self, assembler: Arc<dyn ContextAssembler>) -> Self {
        self.assembler = assembler;
        self
    }
    pub fn catalog(&self) -> &ModelCatalog {
        &self.catalog
    }

    /// A fixed-model auxiliary call (for example, summarization). It uses the
    /// same validation, middleware, cancellation, usage and durable reservations
    /// as ordinary calls, but does not recursively reduce or inject context and
    /// never publishes summary text as the user's answer. No automatic retry.
    pub async fn invoke_auxiliary(
        &self,
        model: &ModelRef,
        request: ModelRequest,
        stage: &str,
        limits: &RunLimits,
        context: &RunContext,
    ) -> Result<ModelResponse, AgentError> {
        limits.validate()?;
        context.check_cancelled()?;
        require_model(&self.catalog, model)?;
        let descriptor = self.catalog.get(model).expect("validated model");
        let budget = ContextBudget {
            max_messages: limits.max_context_messages,
            max_tokens: descriptor.input_capacity(limits, &request.options)?,
        };
        let options = request.options.clone();
        let authorized_tools = request.tools.clone();
        let invocation = ModelInvocationContext {
            model,
            stage,
            step: 0,
            attempt: 1,
            run: context,
        };
        let next = ModelNext {
            middleware: &self.middleware,
            provider: self.catalog.connections[&model.connection]
                .provider
                .as_ref(),
            stream: &SilentStream,
            budget,
            descriptor,
            estimator: self.catalog.connections[&model.connection]
                .token_estimator
                .as_ref(),
            max_total_tokens: limits.max_total_tokens,
            default_output_tokens: limits.max_model_output_tokens,
            run: context,
            target: model,
            authorized_tools: &authorized_tools,
        };
        context
            .emit(AgentEvent::ModelSelected {
                stage: stage.into(),
                model: model.clone(),
                step: 0,
                attempt: 1,
            })
            .await;
        context
            .emit(AgentEvent::ModelStarted {
                step: 0,
                attempt: 1,
            })
            .await;
        let mut response = tokio::select! {
            biased;
            _ = context.cancelled() => return Err(AgentError::Cancelled),
            result = tokio::time::timeout(limits.model_timeout, next.run(request, &invocation)) => {
                result.map_err(|_| ModelError::Transient("auxiliary model invocation timed out".into()))??
            }
        };
        crate::middleware::validate_model_response(&mut response, &options)?;
        if let Some(data) = &mut response.model_context {
            data.model = model.clone();
        }
        context
            .emit(AgentEvent::ModelUsage {
                stage: stage.into(),
                model: model.clone(),
                usage: response.usage.clone(),
            })
            .await;
        Ok(response)
    }

    pub async fn invoke(
        &self,
        call: ModelCall<'_>,
        memory: &mut dyn ConversationSession,
        context: &RunContext,
    ) -> Result<ModelResponse, AgentError> {
        call.limits.validate()?;
        call.options.validate()?;
        let mut retries = 0;
        let mut recoveries = 0;
        let mut attempt = 0;
        let mut previous = None;
        let snapshot = memory.snapshot().await?;
        let mut overflow: Option<(String, Vec<Message>)> = None;
        loop {
            context.check_cancelled()?;
            let consumed = context.usage();
            if consumed.input_tokens.saturating_add(consumed.output_tokens)
                >= call.limits.max_total_tokens
            {
                return Err(AgentError::Run("total model token budget exhausted".into()));
            }
            attempt += 1;
            let selected = self
                .strategy
                .select(
                    &ModelSelectionContext {
                        stage: call.stage,
                        step: call.step,
                        attempt,
                        transient_failures: retries,
                        previous: previous.as_ref(),
                        tools: call.tools,
                        messages: &snapshot.messages,
                        options: call.options,
                        run: context,
                    },
                    &self.catalog,
                )
                .await?;
            require_model(&self.catalog, &selected)?;
            let descriptor = self.catalog.get(&selected).expect("validated model");
            let estimator = self.catalog.connections[&selected.connection]
                .token_estimator
                .as_ref();
            if !call.tools.is_empty() && descriptor.supports_tools == Some(false) {
                return Err(AgentError::InvalidConfiguration(
                    "selected model does not support tools".into(),
                ));
            }
            let full_budget = ContextBudget {
                max_messages: call.limits.max_context_messages,
                max_tokens: descriptor.input_capacity(call.limits, call.options)?,
            };
            let assembled = self
                .assembler
                .assemble(&ContextAssemblyInput {
                    stage: call.stage,
                    attempt,
                    step: call.step,
                    model: &selected,
                    budget: full_budget,
                    estimator,
                    history: &snapshot,
                    instructions: call.instructions,
                    tools: call.tools,
                    options: call.options,
                    run: context,
                    models: self,
                    limits: call.limits,
                    overflow_recoveries: recoveries,
                })
                .await;
            if let Err(error) = &assembled {
                let report = match error {
                    AgentError::ContextAssembly { report, .. } => report.as_ref().clone(),
                    _ => ContextFailureReport::new(
                        selected.clone(),
                        call.stage.into(),
                        call.step,
                        attempt,
                        context.node_execution().cloned(),
                        Vec::new(),
                        error,
                    ),
                };
                context
                    .emit(AgentEvent::ContextAssemblyFailed(report))
                    .await;
            }
            let window = match assembled {
                Ok(window) => window,
                Err(error)
                    if overflow.is_some()
                        && matches!(
                            error.primary(),
                            AgentError::Context(ContextError::BudgetExceeded(_))
                        ) =>
                {
                    return Err(
                        ModelError::ContextOverflow(overflow.as_ref().unwrap().0.clone()).into(),
                    );
                }
                Err(error) => return Err(error),
            };
            let messages = window.messages;
            if let Some((error, previous_messages)) = &overflow {
                if &messages == previous_messages {
                    return Err(ModelError::ContextOverflow(error.clone()).into());
                }
                context.emit(AgentEvent::ContextRecovered).await;
            }
            context
                .emit(AgentEvent::ContextAssembled(window.report))
                .await;
            let attempted_messages = messages.clone();
            context
                .emit(AgentEvent::ModelSelected {
                    stage: call.stage.into(),
                    model: selected.clone(),
                    step: call.step,
                    attempt,
                })
                .await;
            context
                .emit(AgentEvent::ModelStarted {
                    step: call.step,
                    attempt,
                })
                .await;
            let stream = AttemptStream {
                context,
                emitted: AtomicBool::new(false),
                message_id: RunId::new_v4().to_string(),
                finished: AtomicBool::new(false),
            };
            context.emit_output(OutputEvent::StepStarted {
                id: stream.message_id.clone(),
                name: format!("{} / step {} / attempt {}", call.stage, call.step, attempt),
            });
            context.emit_output(OutputEvent::MessageStarted {
                message_id: stream.message_id.clone(),
            });
            let provider = &self.catalog.connections[&selected.connection].provider;
            let invocation_context = ModelInvocationContext {
                model: &selected,
                stage: call.stage,
                step: call.step,
                attempt,
                run: context,
            };
            let next = ModelNext {
                middleware: &self.middleware,
                provider: provider.as_ref(),
                stream: &stream,
                budget: full_budget,
                descriptor,
                estimator,
                max_total_tokens: call.limits.max_total_tokens,
                default_output_tokens: call.limits.max_model_output_tokens,
                run: context,
                target: &selected,
                authorized_tools: call.tools,
            };
            let response = tokio::select! {
                biased;
                _ = context.cancelled() => return Err(AgentError::Cancelled),
                response = tokio::time::timeout(
                call.limits.model_timeout,
                next.run(
                    ModelRequest {
                        messages,
                        tools: call.tools.to_vec(),
                        extensions: context.extensions.clone(),
                        options: call.options.clone(),
                    },
                    &invocation_context,
                ),
            ) => response.unwrap_or_else(|_| Err(ModelError::Transient("model invocation timed out".into())))
            };
            previous = Some(selected.clone());
            match response {
                Ok(mut response) => {
                    crate::middleware::validate_model_response(&mut response, call.options)?;
                    // Provider IDs need only be unique within one response.
                    // Fresh framework identities also isolate providers/middleware
                    // that return clones of a previously constructed response.
                    for tool in &mut response.tool_calls {
                        tool.invocation_id = RunId::new_v4();
                    }
                    if let Some(data) = &mut response.model_context {
                        data.model = selected.clone();
                    }
                    context.emit_output(OutputEvent::MessageFinished {
                        message_id: stream.message_id.clone(),
                        content: response.content.clone(),
                        structured: response.structured.clone(),
                        tool_calls: response.tool_calls.clone(),
                    });
                    stream.finished.store(true, Ordering::Release);
                    context
                        .emit(AgentEvent::ModelUsage {
                            stage: call.stage.into(),
                            model: selected,
                            usage: response.usage.clone(),
                        })
                        .await;
                    return Ok(response);
                }
                Err(ModelError::Transient(_))
                    if !stream.emitted.load(Ordering::Acquire)
                        && retries < call.limits.max_model_retries =>
                {
                    retries += 1;
                    context
                        .emit(AgentEvent::ModelRetry {
                            step: call.step,
                            attempt,
                        })
                        .await;
                    tokio::time::sleep(std::time::Duration::from_millis(
                        10 * retries.min(100) as u64,
                    ))
                    .await;
                }
                Err(ModelError::ContextOverflow(message))
                    if !stream.emitted.load(Ordering::Acquire)
                        && recoveries < call.limits.max_overflow_recoveries =>
                {
                    overflow = Some((message, attempted_messages));
                    recoveries += 1;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

struct AttemptStream<'a> {
    context: &'a RunContext,
    emitted: AtomicBool,
    message_id: String,
    finished: AtomicBool,
}
impl Drop for AttemptStream<'_> {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Acquire) {
            self.context.emit_output(OutputEvent::MessageAborted {
                message_id: self.message_id.clone(),
            });
        }
        self.context.emit_output(OutputEvent::StepFinished {
            id: self.message_id.clone(),
        });
    }
}

struct SilentStream;
#[async_trait]
impl ModelStream for SilentStream {
    async fn text_delta(&self, _: &str) {}
    async fn reasoning_delta(&self, _: &str) {}
}
#[async_trait]
impl ModelStream for AttemptStream<'_> {
    async fn text_delta(&self, text: &str) {
        if !text.is_empty() {
            self.emitted.store(true, Ordering::Release);
            self.context.emit_output(OutputEvent::TextDelta {
                message_id: self.message_id.clone(),
                delta: text.into(),
            });
        }
        self.context.text_delta(text).await;
    }
    async fn reasoning_delta(&self, text: &str) {
        if !text.is_empty() {
            self.emitted.store(true, Ordering::Release);
            self.context.emit_output(OutputEvent::ReasoningDelta {
                message_id: self.message_id.clone(),
                delta: text.into(),
            });
        }
        self.context.reasoning_delta(text).await;
    }
}

pub(crate) struct LegacyModelProvider(pub Arc<dyn Model>);
#[async_trait]
impl ModelProvider for LegacyModelProvider {
    async fn generate(
        &self,
        _: &str,
        request: ModelRequest,
        stream: &dyn ModelStream,
    ) -> Result<ModelResponse, ModelError> {
        self.0.generate(request, stream).await
    }
}
