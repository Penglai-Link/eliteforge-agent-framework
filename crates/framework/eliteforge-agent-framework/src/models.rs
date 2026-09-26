use crate::*;
use async_trait::async_trait;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
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
        let strategy = strategy.unwrap_or_else(|| Arc::new(CatalogModelStrategy::default()));
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
        request: ChatRequest,
        stage: &str,
        limits: &RunLimits,
        context: &RunContext,
    ) -> Result<ChatResponse, AgentError> {
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
            provider: self.catalog.adapter(model).expect("validated model"),
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
            data.protocol = self
                .catalog
                .protocol(model)
                .expect("validated model")
                .into();
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
    ) -> Result<ChatResponse, AgentError> {
        call.limits.validate()?;
        call.options.validate()?;
        let mut retries = 0;
        let mut recoveries = 0;
        let mut attempt = 0;
        let mut previous = None;
        let snapshot = memory.snapshot().await?;
        let selection_messages: Vec<_> = call
            .instructions
            .iter()
            .chain(&snapshot.messages)
            .cloned()
            .collect();
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
            let selected = if let Some(model) = &context.model {
                model.clone()
            } else {
                self.strategy
                    .select(
                        &ModelSelectionContext {
                            stage: call.stage,
                            step: call.step,
                            attempt,
                            transient_failures: retries,
                            previous: previous.as_ref(),
                            tools: call.tools,
                            messages: &selection_messages,
                            options: call.options,
                            run: context,
                        },
                        &self.catalog,
                    )
                    .await?
            };
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
            let provider = self.catalog.adapter(&selected).expect("validated model");
            let invocation_context = ModelInvocationContext {
                model: &selected,
                stage: call.stage,
                step: call.step,
                attempt,
                run: context,
            };
            let next = ModelNext {
                middleware: &self.middleware,
                provider,
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
                    ChatRequest {
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
                        data.protocol = self
                            .catalog
                            .protocol(&selected)
                            .expect("validated model")
                            .into();
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
impl ChatStream for SilentStream {
    async fn text_delta(&self, _: &str) {}
    async fn reasoning_delta(&self, _: &str) {}
}
#[async_trait]
impl ChatStream for AttemptStream<'_> {
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

#[async_trait]
impl ChatExecutor for ModelInvoker {
    async fn validate_async(&self) -> Result<(), AgentError> {
        self.strategy.validate_async(&self.catalog).await
    }
    fn catalog(&self) -> &ModelCatalog {
        self.catalog()
    }
    async fn invoke(
        &self,
        call: ModelCall<'_>,
        memory: &mut dyn ConversationSession,
        context: &RunContext,
    ) -> Result<ChatResponse, AgentError> {
        self.invoke(call, memory, context).await
    }
    async fn invoke_auxiliary(
        &self,
        model: &ModelRef,
        request: ChatRequest,
        stage: &str,
        limits: &RunLimits,
        context: &RunContext,
    ) -> Result<ChatResponse, AgentError> {
        self.invoke_auxiliary(model, request, stage, limits, context)
            .await
    }
}
