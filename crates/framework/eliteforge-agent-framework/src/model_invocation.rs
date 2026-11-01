//! Direct model execution. No Agent construction, native run, session, or loop.
use crate::*;
use std::sync::Arc;

impl ModelClient {
    /// One-shot automatic text generation. Candidates come from the registered
    /// catalog and the host data source; no Agent/session/history is created.
    /// Fixed models remain available through `chat`.
    pub async fn text_generation(
        &self,
        request: ChatRequest,
        limits: &RunLimits,
        stage: &str,
    ) -> Result<ChatResponse, AgentError> {
        limits.validate()?;
        let snapshot = self.runtime.snapshot();
        let candidates = snapshot
            .chat
            .models()
            .into_iter()
            .filter_map(|model| {
                let descriptor = snapshot.chat.get(&model)?;
                Some(AutoRouteCandidate {
                    protocol: snapshot.chat.protocol(&model).unwrap_or_default().into(),
                    context_window_tokens: descriptor.context_window_tokens,
                    input_modalities: descriptor.input_modalities.clone(),
                    metadata: descriptor.metadata.clone(),
                    model,
                })
            })
            .collect::<Vec<_>>();
        let call_key = RunId::new_v4().to_string();
        let model = self.auto_routers.select_text_generation(
            &AutoRouteContext {
                operation: ModelOperation::Chat,
                stage,
                affinity: &call_key,
                call_key: &call_key,
                previous: None,
                transient_failures: 0,
            },
            &candidates,
        )?;
        let mut pinned = self.clone();
        pinned.runtime = ModelRuntime::frozen(snapshot);
        pinned.chat(&model, request, limits, stage).await
    }

    pub fn control_context(&self, limits: &RunLimits) -> Result<RunContext, AgentError> {
        limits.validate()?;
        let mut context = match &self.options.run {
            Some(context) => context.clone(),
            None => ModelInvocationOptions::for_call(limits)?
                .run
                .expect("model control data"),
        };
        // Model calls share the caller's accounting and cancellation, never
        // its answer publication. Internal summaries/judges stay in the return
        // value and observed report instead of becoming parent chat messages.
        let (events, _) = tokio::sync::broadcast::channel(1);
        context.events = events;
        context.publication = Arc::default();
        context.sequence = Arc::new(std::sync::atomic::AtomicU64::new(1));
        context.observers = None;
        context.sinks.clear();
        context.resources = Arc::default();
        let mut effective = limits.clone();
        let parent = context.limits();
        effective.max_model_calls_per_step = match (
            effective.max_model_calls_per_step,
            parent.max_model_calls_per_step,
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        effective.max_model_retries = effective.max_model_retries.min(parent.max_model_retries);
        effective.max_phase_continuations = effective
            .max_phase_continuations
            .min(parent.max_phase_continuations);
        effective.max_overflow_recoveries = effective
            .max_overflow_recoveries
            .min(parent.max_overflow_recoveries);
        effective.cleanup_timeout = effective.cleanup_timeout.min(parent.cleanup_timeout);
        effective.max_context_tokens = effective.max_context_tokens.min(parent.max_context_tokens);
        effective.max_context_messages = effective
            .max_context_messages
            .min(parent.max_context_messages);
        effective.max_model_output_tokens = effective
            .max_model_output_tokens
            .min(parent.max_model_output_tokens);
        effective.max_total_tokens = effective.max_total_tokens.min(parent.max_total_tokens);
        effective.model_timeout = effective
            .model_timeout
            .min(parent.model_timeout)
            .min(self.options.timeout);
        effective.run_timeout = effective.run_timeout.min(parent.run_timeout);
        context.token_ceiling = context.token_ceiling.min(effective.max_total_tokens);
        context.deadline = context
            .deadline
            .min(tokio::time::Instant::now() + effective.run_timeout);
        context.limits = effective;
        context.cancellation = context.cancellation.child_token();
        Ok(context)
    }

    fn direct_invoker(
        &self,
        model: &ModelRef,
        limits: &RunLimits,
        context: &RunContext,
    ) -> Result<ModelInvoker, AgentError> {
        let snapshot = self.runtime.snapshot();
        ModelInvoker::new(
            (*snapshot.chat).clone(),
            Some(Arc::new(FixedModelStrategy(model.clone()))),
        )?
        .with_call_limit(
            limits
                .max_model_calls_per_step
                .unwrap_or(limits.max_model_retries.saturating_add(2)),
        )
        .map(|invoker| invoker.with_control_context(context.clone()))
    }

    pub async fn chat(
        &self,
        model: &ModelRef,
        request: ChatRequest,
        limits: &RunLimits,
        stage: &str,
    ) -> Result<ChatResponse, AgentError> {
        self.chat_observed(model, request, limits, stage)
            .await
            .into_result()
    }
    pub async fn chat_observed(
        &self,
        model: &ModelRef,
        request: ChatRequest,
        limits: &RunLimits,
        stage: &str,
    ) -> ObservedModelResult<ChatResponse> {
        self.chat_direct(model, request, limits, stage, None).await
    }
    pub async fn chat_with_validator_observed(
        &self,
        model: &ModelRef,
        request: ChatRequest,
        limits: &RunLimits,
        stage: &str,
        validator: Arc<dyn ModelResponseValidator>,
    ) -> ObservedModelResult<ChatResponse> {
        self.chat_direct(model, request, limits, stage, Some(validator))
            .await
    }
    async fn chat_direct(
        &self,
        model: &ModelRef,
        mut request: ChatRequest,
        limits: &RunLimits,
        stage: &str,
        validator: Option<Arc<dyn ModelResponseValidator>>,
    ) -> ObservedModelResult<ChatResponse> {
        let report = Arc::new(model_observation::ModelReportState::default());
        let prepared = self.control_context(limits).and_then(|mut context| {
            context.model = Some(model.clone());
            // Freeze initial standalone admission once. An inherited parent is
            // authoritative; validator input may neither replace nor remove it.
            if context.extensions.get::<ContinuationBudget>().is_none()
                && let Some(budget) = request.extensions.get::<ContinuationBudget>()
            {
                context.extensions.insert((*budget).clone());
            }
            if context.extensions.get::<ModelDispatchCounter>().is_none()
                && let Some(counter) = request.extensions.get::<ModelDispatchCounter>()
            {
                context.extensions.insert((*counter).clone());
            }
            context.extensions.insert(ModelExecutionScope::new(
                self.runtime.clone(),
                Arc::new(FixedModelStrategy(model.clone())),
            )?);
            let context = report.attach(&context);
            let invoker = self.direct_invoker(model, context.limits(), &context)?;
            Ok((context, invoker))
        });
        let result = match prepared {
            Err(error) => Err(error),
            Ok((context, invoker)) => {
                let execution = async {
                    let mut validations = 0;
                    loop {
                        context.check_cancelled()?;
                        let mut invocation = context.clone();
                        invocation.extensions = request.extensions.clone();
                        // Public request extensions may change after validation. Execution
                        // authority and accounting remain bound to the original call.
                        if let Some(budget) = context.extensions.get::<ContinuationBudget>() {
                            invocation.extensions.insert((*budget).clone());
                        }
                        if let Some(counter) = context.extensions.get::<ModelDispatchCounter>() {
                            invocation.extensions.insert((*counter).clone());
                        }
                        if let Some(scope) = context.extensions.get::<ModelExecutionScope>() {
                            invocation.extensions.insert((*scope).clone());
                        }
                        invocation.extensions.insert(report.clone());
                        let mut projection = TranscriptProjection::new(request.messages.clone());
                        let response = invoker
                            .clone()
                            .with_control_context(invocation.clone())
                            .invoke(
                                ModelCall {
                                    stage,
                                    step: validations,
                                    instructions: &[],
                                    tools: &request.tools,
                                    limits: context.limits(),
                                    options: &request.options,
                                },
                                &mut projection,
                                &invocation,
                            )
                            .await?;
                        if let Some(validator) = &validator {
                            validations += 1;
                            if let Some(next) = validator.validate(&response, validations).await? {
                                request = next;
                                continue;
                            }
                        }
                        return Ok(response);
                    }
                };
                self.controlled(&context, execution).await
            }
        };
        ObservedModelResult {
            result,
            report: report.snapshot(report.calls()),
        }
    }

    pub async fn workflow_observed(
        &self,
        model: &ModelRef,
        workflow: Arc<dyn ModelWorkflow>,
        limits: &RunLimits,
        _stage: &str,
    ) -> ObservedModelResult<ChatResponse> {
        let report = Arc::new(model_observation::ModelReportState::default());
        let prepared = self.control_context(limits).and_then(|mut context| {
            context.model = Some(model.clone());
            context.extensions.insert(ModelExecutionScope::new(
                self.runtime.clone(),
                Arc::new(FixedModelStrategy(model.clone())),
            )?);
            let context = report.attach(&context);
            let invoker = self.direct_invoker(model, context.limits(), &context)?;
            Ok((context, invoker))
        });
        let result = match prepared {
            Err(error) => Err(error),
            Ok((context, invoker)) => {
                self.controlled(
                    &context,
                    workflow.run(&context, &invoker, context.limits(), model),
                )
                .await
            }
        };
        ObservedModelResult {
            result,
            report: report.snapshot(report.calls()),
        }
    }

    pub async fn embed_observed(
        &self,
        mut request: EmbeddingRequest,
        limits: &RunLimits,
    ) -> ObservedModelResult<EmbeddingResponse> {
        let report = Arc::new(model_observation::ModelReportState::default());
        let prepared = self.control_context(limits).and_then(|context| {
            let mut context = report.attach(&context);
            context.extensions.insert(ModelCallLimit::new(
                context
                    .limits()
                    .max_model_calls_per_step
                    .unwrap_or(context.limits().max_model_retries.saturating_add(2)),
            )?);
            let snapshot = self.runtime.snapshot();
            request.choice = self.route_choice(
                ModelOperation::Embedding,
                &snapshot.embedding,
                &request.choice,
                &request.inputs,
            )?;
            let binding = snapshot
                .embedding
                .select(&request.choice, &request.inputs)?
                .clone();
            request.choice.model = Some(binding.model.clone());
            let mut estimate_request = request.clone();
            if let Some(space) = snapshot.spaces.get(&binding.model) {
                estimate_request.dimensions = Some(space.dimensions);
            }
            let retry_input = binding
                .implementation
                .input_token_estimate(&estimate_request);
            Ok((context, binding, retry_input, snapshot))
        });
        let result = match prepared {
            Err(error) => Err(error),
            Ok((context, binding, retry_input, snapshot)) => {
                let mut options = ModelInvocationOptions::for_run(&context, context.limits());
                options.max_input_bytes = self.options.max_input_bytes;
                options.max_batch_size = self.options.max_batch_size;
                let mut client = self
                    .with_runtime(ModelRuntime::frozen(snapshot))
                    .with_options(options);
                client.admitted = true;
                let execution = async {
                    let max = context
                        .limits()
                        .max_model_calls_per_step
                        .unwrap_or(context.limits().max_model_retries.saturating_add(2));
                    let mut attempts = 0;
                    let mut refreshed = false;
                    loop {
                        if attempts >= max {
                            return Err(AgentError::Run("model call budget exhausted".into()));
                        }
                        context.check_cancelled()?;
                        attempts += 1;
                        match ModelOperations::embed(&client, request.clone()).await {
                            Ok(response) => {
                                report.operation_usage(response.input_tokens.map(|input_tokens| {
                                    Usage {
                                        input_tokens,
                                        output_tokens: 0,
                                    }
                                }));
                                return Ok(response);
                            }
                            Err(AgentError::Model(ModelError::Provider(ref detail)))
                                if detail.category == ProviderErrorCategory::Authentication
                                    && !refreshed
                                    && attempts < max
                                    && context
                                        .usage
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .can_reserve(retry_input, context.token_ceiling)
                                    && context
                                        .extensions
                                        .get::<ContinuationBudget>()
                                        .is_none_or(|budget| {
                                            budget.remaining_model_calls() > 0
                                        }) =>
                            {
                                refreshed = true;
                                let error = ModelError::Provider(detail.clone());
                                let recovered = tokio::time::timeout(
                                    context.limits().model_timeout,
                                    binding.implementation.recover_authentication(&error),
                                )
                                .await
                                .unwrap_or_else(|_| {
                                    Err(ModelError::Fatal("credential refresh timed out".into()))
                                });
                                match recovered {
                                    Ok(true) => {}
                                    Ok(false) => {
                                        binding.implementation.on_failure(&error);
                                        return Err(error.into());
                                    }
                                    Err(failure) => {
                                        binding.implementation.on_failure(&failure);
                                        return Err(failure.into());
                                    }
                                }
                            }
                            Err(AgentError::Model(ref error))
                                if error.is_transient()
                                    && attempts <= context.limits().max_model_retries
                                    && attempts < max => {}
                            Err(error) => {
                                if let AgentError::Model(model_error) = error.primary() {
                                    binding.implementation.on_failure(model_error);
                                }
                                return Err(error);
                            }
                        }
                        context
                            .emit(AgentEvent::ModelRetry {
                                step: 0,
                                attempt: attempts,
                            })
                            .await;
                    }
                };
                self.controlled(&context, execution).await
            }
        };
        ObservedModelResult {
            result,
            report: report.snapshot(report.calls()),
        }
    }

    pub async fn rerank_observed(
        &self,
        mut request: RerankRequest,
        limits: &RunLimits,
    ) -> ObservedModelResult<RerankResponse> {
        let report = Arc::new(model_observation::ModelReportState::default());
        let prepared = self.control_context(limits).and_then(|context| {
            let mut context = report.attach(&context);
            context.extensions.insert(ModelCallLimit::new(
                context
                    .limits()
                    .max_model_calls_per_step
                    .unwrap_or(context.limits().max_model_retries.saturating_add(2)),
            )?);
            let mut inputs = request.documents.clone();
            inputs.push(request.query.clone());
            let snapshot = self.runtime.snapshot();
            request.choice = self.route_choice(
                ModelOperation::Rerank,
                &snapshot.rerank,
                &request.choice,
                &inputs,
            )?;
            let binding = snapshot.rerank.select(&request.choice, &inputs)?.clone();
            request.choice.model = Some(binding.model.clone());
            let mut estimate_request = request.clone();
            estimate_request.choice.model = Some(binding.model.clone());
            let retry_input = binding
                .implementation
                .input_token_estimate(&estimate_request);
            Ok((context, binding, retry_input, snapshot))
        });
        let result = match prepared {
            Err(error) => Err(error),
            Ok((context, binding, retry_input, snapshot)) => {
                let mut options = ModelInvocationOptions::for_run(&context, context.limits());
                options.max_input_bytes = self.options.max_input_bytes;
                options.max_batch_size = self.options.max_batch_size;
                let mut client = self
                    .with_runtime(ModelRuntime::frozen(snapshot))
                    .with_options(options);
                client.admitted = true;
                let execution = async {
                    let max = context
                        .limits()
                        .max_model_calls_per_step
                        .unwrap_or(context.limits().max_model_retries.saturating_add(2));
                    let mut attempts = 0;
                    let mut refreshed = false;
                    loop {
                        if attempts >= max {
                            return Err(AgentError::Run("model call budget exhausted".into()));
                        }
                        context.check_cancelled()?;
                        attempts += 1;
                        match ModelOperations::rerank(&client, request.clone()).await {
                            Ok(response) => return Ok(response),
                            Err(AgentError::Model(ModelError::Provider(ref detail)))
                                if detail.category == ProviderErrorCategory::Authentication
                                    && !refreshed
                                    && attempts < max
                                    && context
                                        .usage
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .can_reserve(retry_input, context.token_ceiling)
                                    && context
                                        .extensions
                                        .get::<ContinuationBudget>()
                                        .is_none_or(|budget| {
                                            budget.remaining_model_calls() > 0
                                        }) =>
                            {
                                refreshed = true;
                                let error = ModelError::Provider(detail.clone());
                                let recovered = tokio::time::timeout(
                                    context.limits().model_timeout,
                                    binding.implementation.recover_authentication(&error),
                                )
                                .await
                                .unwrap_or_else(|_| {
                                    Err(ModelError::Fatal("credential refresh timed out".into()))
                                });
                                match recovered {
                                    Ok(true) => {}
                                    Ok(false) => {
                                        binding.implementation.on_failure(&error);
                                        return Err(error.into());
                                    }
                                    Err(failure) => {
                                        binding.implementation.on_failure(&failure);
                                        return Err(failure.into());
                                    }
                                }
                            }
                            Err(AgentError::Model(ref error))
                                if error.is_transient()
                                    && attempts <= context.limits().max_model_retries
                                    && attempts < max => {}
                            Err(error) => {
                                if let AgentError::Model(model_error) = error.primary() {
                                    binding.implementation.on_failure(model_error);
                                }
                                return Err(error);
                            }
                        }
                        context
                            .emit(AgentEvent::ModelRetry {
                                step: 0,
                                attempt: attempts,
                            })
                            .await;
                    }
                };
                self.controlled(&context, execution).await
            }
        };
        ObservedModelResult {
            result,
            report: report.snapshot(report.calls()),
        }
    }
}
