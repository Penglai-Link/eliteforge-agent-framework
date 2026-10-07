use crate::*;
use async_trait::async_trait;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[derive(Clone)]
pub struct ModelInvoker {
    catalog: ModelCatalog,
    runtime: Option<Arc<ModelRuntime>>,
    strategy: Arc<dyn ModelSelectionStrategy>,
    middleware: Vec<Arc<dyn ModelMiddleware>>,
    assembler: Arc<dyn ContextAssembler>,
    recovery: ModelRecoveryPolicy,
    refreshers: Vec<Arc<dyn ModelCatalogRefresh>>,
    call_limit: Option<ModelCallLimit>,
    control: Option<RunContext>,
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
            runtime: None,
            strategy,
            middleware: Vec::new(),
            assembler: Arc::new(DefaultContextAssembler::default()),
            recovery: ModelRecoveryPolicy::default(),
            refreshers: Vec::new(),
            call_limit: None,
            control: None,
        })
    }
    pub fn with_call_limit(mut self, maximum: usize) -> Result<Self, AgentError> {
        self.call_limit = Some(ModelCallLimit::new(maximum)?);
        Ok(self)
    }
    pub fn with_control_context(mut self, context: RunContext) -> Self {
        self.control = Some(context);
        self
    }
    fn control_context(&self, supplied: &RunContext) -> RunContext {
        let mut context = self.control.clone().unwrap_or_else(|| supplied.clone());
        if let Some(limit) = &self.call_limit {
            context.extensions.insert(limit.clone());
        }
        context
    }
    fn control_limits(&self, supplied: &RunLimits, context: &RunContext) -> RunLimits {
        let mut limits = supplied.clone();
        if self.control.is_some() {
            let bound = context.limits();
            limits.max_context_tokens = limits.max_context_tokens.min(bound.max_context_tokens);
            limits.max_context_messages =
                limits.max_context_messages.min(bound.max_context_messages);
            limits.max_model_output_tokens = limits
                .max_model_output_tokens
                .min(bound.max_model_output_tokens);
            limits.max_total_tokens = limits.max_total_tokens.min(bound.max_total_tokens);
            limits.max_model_retries = limits.max_model_retries.min(bound.max_model_retries);
            limits.max_overflow_recoveries = limits
                .max_overflow_recoveries
                .min(bound.max_overflow_recoveries);
            limits.cleanup_timeout = limits.cleanup_timeout.min(bound.cleanup_timeout);
            limits.model_timeout = limits.model_timeout.min(bound.model_timeout);
            limits.max_model_calls_per_step = match (
                limits.max_model_calls_per_step,
                bound.max_model_calls_per_step,
            ) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        limits
    }
    pub fn with_runtime(mut self, runtime: Arc<ModelRuntime>) -> Self {
        self.runtime = Some(runtime);
        self
    }
    pub fn with_recovery(
        mut self,
        policy: ModelRecoveryPolicy,
        refreshers: Vec<Arc<dyn ModelCatalogRefresh>>,
    ) -> Self {
        self.recovery = policy;
        self.refreshers = refreshers;
        self
    }
    pub fn catalog_snapshot(&self) -> Arc<ModelCatalog> {
        self.runtime.as_ref().map_or_else(
            || Arc::new(self.catalog.clone()),
            |runtime| runtime.snapshot().chat.clone(),
        )
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

    fn scoped(&self, context: &RunContext) -> Self {
        let Some(scope) = context.extensions.get::<ModelExecutionScope>() else {
            return self.clone();
        };
        Self {
            catalog: (*scope.runtime.snapshot().chat).clone(),
            runtime: Some(scope.runtime.clone()),
            strategy: scope.strategy.clone(),
            recovery: scope.recovery.clone(),
            refreshers: scope.refreshers.clone(),
            middleware: self.middleware.clone(),
            assembler: self.assembler.clone(),
            call_limit: self.call_limit.clone(),
            control: self.control.clone(),
        }
    }

    fn notify_unavailable(&self, model: &ModelRef) {
        for service in &self.refreshers {
            if service.connection_id() == model.connection {
                service.request_refresh();
            }
        }
    }

    async fn recover_unavailable(
        &self,
        selected: &ModelRef,
        messages: &[Message],
        call: &ModelCall<'_>,
        context: &RunContext,
        state: &mut crate::model_recovery::RecoveryState,
    ) -> Result<Option<(Arc<ModelCatalog>, ModelRef)>, AgentError> {
        if self.recovery.candidates.is_empty()
            || (context.model.is_some() && !self.recovery.allow_explicit_model_replacement)
        {
            return Ok(None);
        }
        state.failed.push(selected.clone());
        state.check_account(selected)?;
        if !state.refreshed {
            state.refreshed = true;
            if let Some(service) = self
                .refreshers
                .iter()
                .find(|service| service.connection_id() == selected.connection)
            {
                let update = tokio::select! {
                    biased;
                    _ = context.cancelled() => return Err(AgentError::Cancelled),
                    result = tokio::time::timeout(call.limits.model_timeout, service.refresh()) => {
                        result.map_err(|_| ModelError::Transient("model catalog refresh timed out".into()))??
                    }
                };
                state.check_account(selected)?;
                if let Some(runtime) = &self.runtime {
                    let accepted = runtime.replace_connection_if_current(
                        service.connection_id(),
                        &update.account_scope,
                        update.revision,
                        update.clone(),
                    )?;
                    if !accepted {
                        return Ok(None);
                    }
                }
            }
        }
        let catalog = self.catalog_snapshot();
        for candidate in &self.recovery.candidates {
            if state.failed.contains(candidate) {
                continue;
            }
            state.check_account(candidate)?;
            if !catalog.get(candidate).is_some_and(|descriptor| {
                let mut options = call.options.clone();
                options.max_output_tokens =
                    Some(descriptor.output_limit(call.limits.max_model_output_tokens, &options));
                descriptor.accepts(messages, call.tools, &options)
                    && descriptor.input_capacity(call.limits, &options).is_ok()
            }) {
                continue;
            }
            context.emit_output(OutputEvent::Custom {
                name: "framework.model_recovery".into(),
                value: serde_json::json!({ "from": selected, "to": candidate, "step": call.step }),
            });
            return Ok(Some((catalog, candidate.clone())));
        }
        Ok(None)
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
        let context = self.control_context(context);
        let limits = self.control_limits(limits, &context);
        let scoped = self.scoped(&context);
        let snapshot = scoped.catalog_snapshot();
        scoped
            .invoke_auxiliary_in_catalog(&snapshot, model, request, stage, &limits, &context)
            .await
    }

    /// Context preparation shares the answer invocation's immutable bindings,
    /// including its authorization context, even if discovery refreshes meanwhile.
    pub(crate) async fn invoke_auxiliary_in_catalog(
        &self,
        catalog: &ModelCatalog,
        model: &ModelRef,
        request: ChatRequest,
        stage: &str,
        limits: &RunLimits,
        context: &RunContext,
    ) -> Result<ChatResponse, AgentError> {
        limits.validate()?;
        context.check_cancelled()?;
        if catalog.get(model).is_none() {
            self.notify_unavailable(model);
            return Err(
                ModelError::Unavailable(format!("{}/{}", model.connection, model.model)).into(),
            );
        }
        let descriptor = catalog.get(model).expect("validated model");
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
            provider: catalog.adapter(model).expect("validated model"),
            stream: &SilentStream,
            budget,
            descriptor,
            estimator: catalog.connections[&model.connection]
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
        let request_extensions = request.extensions.clone();
        let result = tokio::select! {
            biased;
            _ = context.cancelled() => return Err(AgentError::Cancelled),
            result = tokio::time::timeout(limits.model_timeout, next.run(request, &invocation)) => {
                result.unwrap_or_else(|_| Err(ModelError::Indeterminate("auxiliary model invocation timed out".into())))
            }
        };
        let mut response = match result {
            Ok(response) => response,
            Err(error) => {
                catalog
                    .adapter(model)
                    .expect("validated model")
                    .on_failure(&request_extensions, &error);
                return Err(error.into());
            }
        };
        crate::middleware::validate_model_response(&mut response, &options)?;
        if let Some(report) = context
            .extensions
            .get::<Arc<crate::model_observation::ModelReportState>>()
        {
            report.response(&response);
        }
        if let Some(data) = &mut response.model_context {
            data.model = model.clone();
            data.protocol = catalog.protocol(model).expect("validated model").into();
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
        let context = self.control_context(context);
        let limits = self.control_limits(call.limits, &context);
        self.scoped(&context)
            .invoke_in_scope(
                ModelCall {
                    limits: &limits,
                    ..call
                },
                memory,
                &context,
            )
            .await
    }

    async fn invoke_in_scope(
        &self,
        call: ModelCall<'_>,
        memory: &mut dyn ConversationSession,
        context: &RunContext,
    ) -> Result<ChatResponse, AgentError> {
        call.limits.validate()?;
        call.options.validate()?;
        let runtime_snapshot = self.runtime.as_ref().map(|runtime| runtime.snapshot());
        let mut catalog_snapshot = runtime_snapshot.as_ref().map_or_else(
            || Arc::new(self.catalog.clone()),
            |snapshot| snapshot.chat.clone(),
        );
        let mut replacement: Option<ModelRef> = None;
        let mut recovery = crate::model_recovery::RecoveryState::new(&self.refreshers);
        let mut retries = 0;
        let mut authentication_recovered = false;
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
        let mut models = crate::model_call_budget::StepModels {
            inner: self,
            catalog_snapshot: catalog_snapshot.clone(),
            limit: match (
                call.limits.max_model_calls_per_step,
                context.limits().max_model_calls_per_step,
            ) {
                (Some(call), Some(run)) => Some(call.min(run)),
                (call, run) => call.or(run),
            },
            used: std::sync::atomic::AtomicUsize::new(0),
        };
        let mut overflow: Option<(String, Vec<Message>)> = None;
        loop {
            let catalog = catalog_snapshot.as_ref();
            context.check_cancelled()?;
            let consumed = context.usage();
            if consumed.input_tokens.saturating_add(consumed.output_tokens)
                >= call.limits.max_total_tokens
            {
                return Err(AgentError::Run("total model token budget exhausted".into()));
            }
            attempt += 1;
            let selected = if let Some(model) = &replacement {
                model.clone()
            } else if let Some(model) = &context.model {
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
                        catalog,
                    )
                    .await?
            };
            if catalog.get(&selected).is_none() {
                self.notify_unavailable(&selected);
                if retries < call.limits.max_model_retries
                    && let Some((fresh, candidate)) = self
                        .recover_unavailable(
                            &selected,
                            &selection_messages,
                            &call,
                            context,
                            &mut recovery,
                        )
                        .await?
                {
                    retries += 1;
                    catalog_snapshot = fresh;
                    models.catalog_snapshot = catalog_snapshot.clone();
                    replacement = Some(candidate);
                    continue;
                }
                return Err(ModelError::Unavailable(format!(
                    "{}/{}",
                    selected.connection, selected.model
                ))
                .into());
            }
            let descriptor = catalog.get(&selected).expect("validated model");
            let mut selected_options = call.options.clone();
            selected_options.max_output_tokens = Some(
                descriptor.output_limit(call.limits.max_model_output_tokens, &selected_options),
            );
            let call = ModelCall {
                stage: call.stage,
                step: call.step,
                instructions: call.instructions,
                tools: call.tools,
                limits: call.limits,
                options: &selected_options,
            };
            let estimator = catalog.connections[&selected.connection]
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
                    models: &models,
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
            models.reserve(false)?;
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
            let provider = catalog.adapter(&selected).expect("validated model");
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
            ) => response.unwrap_or_else(|_| Err(ModelError::Indeterminate("model invocation timed out".into())))
            };
            if response.as_ref().is_err_and(|error| error.is_unavailable()) {
                self.notify_unavailable(&selected);
            }
            previous = Some(selected.clone());
            match response {
                Ok(mut response) => {
                    crate::middleware::validate_model_response(&mut response, call.options)?;
                    if let Some(report) = context
                        .extensions
                        .get::<Arc<crate::model_observation::ModelReportState>>()
                    {
                        report.response(&response);
                    }
                    // Provider IDs need only be unique within one response.
                    // Fresh framework identities also isolate providers/middleware
                    // that return clones of a previously constructed response.
                    for tool in &mut response.tool_calls {
                        tool.invocation_id = RunId::new_v4();
                    }
                    if let Some(data) = &mut response.model_context {
                        data.model = selected.clone();
                        data.protocol =
                            catalog.protocol(&selected).expect("validated model").into();
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
                Err(ModelError::Provider(ref error))
                    if error.category == ProviderErrorCategory::Authentication
                        && !authentication_recovered
                        && !stream.emitted.load(Ordering::Acquire) =>
                {
                    let authentication_error = ModelError::Provider(error.clone());
                    // Refreshing credentials has side effects. Do not refresh when
                    // any authoritative admission ceiling forbids another dispatch.
                    let no_capacity = self
                        .call_limit
                        .as_ref()
                        .is_some_and(|limit| limit.remaining() == 0)
                        || models
                            .limit
                            .is_some_and(|limit| models.used.load(Ordering::Acquire) >= limit)
                        || context
                            .extensions
                            .get::<ModelCallLimit>()
                            .is_some_and(|limit| limit.remaining() == 0)
                        || context
                            .extensions
                            .get::<ContinuationBudget>()
                            .is_some_and(|budget| budget.remaining_model_calls() == 0);
                    let retry_request = ChatRequest {
                        messages: attempted_messages.clone(),
                        tools: call.tools.to_vec(),
                        extensions: context.extensions.clone(),
                        options: call.options.clone(),
                    };
                    let input = if no_capacity {
                        None
                    } else {
                        tokio::select! {
                            biased;
                            _ = context.cancelled() => return Err(AgentError::Cancelled),
                            result = tokio::time::timeout(call.limits.model_timeout, estimator.request(&selected, &retry_request)) => result.ok().and_then(Result::ok),
                        }
                    };
                    let token_capacity = input.is_some_and(|input| {
                        context
                            .usage
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .can_reserve(
                                input.min(u64::MAX as usize) as u64,
                                context.token_ceiling.min(call.limits.max_total_tokens),
                            )
                    });
                    if no_capacity || !token_capacity {
                        provider.on_failure(&context.extensions, &authentication_error);
                        return Err(authentication_error.into());
                    }
                    authentication_recovered = true;
                    let refreshed = tokio::select! {
                        biased;
                        _ = context.cancelled() => return Err(AgentError::Cancelled),
                        result = tokio::time::timeout(call.limits.model_timeout, provider.recover_authentication(&context.extensions, &authentication_error)) => result.unwrap_or_else(|_|Err(ModelError::Fatal("credential refresh timed out".into()))),
                    };
                    let refreshed = match refreshed {
                        Ok(value) => value,
                        Err(error) => {
                            provider.on_failure(&context.extensions, &authentication_error);
                            return Err(error.into());
                        }
                    };
                    if !refreshed {
                        let error = ModelError::Provider(error.clone());
                        provider.on_failure(&context.extensions, &error);
                        return Err(error.into());
                    }
                    context
                        .emit(AgentEvent::ModelRetry {
                            step: call.step,
                            attempt,
                        })
                        .await;
                }
                Err(error)
                    if error.is_unavailable()
                        && !stream.emitted.load(Ordering::Acquire)
                        && retries < call.limits.max_model_retries =>
                {
                    let Some((fresh, candidate)) = self
                        .recover_unavailable(
                            &selected,
                            &selection_messages,
                            &call,
                            context,
                            &mut recovery,
                        )
                        .await?
                    else {
                        provider.on_failure(&context.extensions, &error);
                        return Err(error.into());
                    };
                    retries += 1;
                    catalog_snapshot = fresh;
                    models.catalog_snapshot = catalog_snapshot.clone();
                    replacement = Some(candidate);
                    overflow = None;
                }
                Err(ref error)
                    if error.is_transient()
                        && !stream.emitted.load(Ordering::Acquire)
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
                Err(error) => {
                    provider.on_failure(&context.extensions, &error);
                    return Err(error.into());
                }
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
        self.strategy.validate_async(&self.catalog_snapshot()).await
    }
    fn catalog(&self) -> &ModelCatalog {
        self.catalog()
    }
    fn catalog_snapshot(&self) -> Arc<ModelCatalog> {
        self.catalog_snapshot()
    }
    fn catalog_for_run(&self, context: &RunContext) -> Arc<ModelCatalog> {
        self.scoped(context).catalog_snapshot()
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
