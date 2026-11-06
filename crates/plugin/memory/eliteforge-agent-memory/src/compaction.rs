//! The memory plugin owns compaction; the context plugin supplies its reducer.
pub use eliteforge_agent_context::{
    CompressionOptions, ConversationSummary, ModelCompressor, SummaryKey, SummaryStore,
    SummaryStorePlugin, SummaryStoreService, VersionedSummary,
};
use eliteforge_agent_core::*;
use std::sync::{Arc, Mutex};

/// Operator and retention metadata is model data, never summary instructions.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MemoryCompactionMetadata {
    pub reason: Option<String>,
    pub note: Option<String>,
    pub retained_recent: Option<String>,
}
impl MemoryCompactionMetadata {
    pub fn as_record(&self) -> String {
        serde_json::json!({"memory_compaction_metadata":self,"retained_recent_policy":"Recent records are replayed separately; use them for continuity without duplicating their transcript."}).to_string()
    }
}
#[derive(Debug, Clone)]
pub struct MemoryCompactionResult {
    pub summary: String,
    /// Conservative run-ledger charge, including reservations for unknown usage.
    pub usage: Usage,
    /// Actual provider usage only when every summary response reports usage.
    pub provider_usage: Option<Usage>,
    pub provider_metrics: Option<ProviderMetrics>,
}
impl MemoryCompactionResult {
    /// Carry the typed memory result through the SDK model-workflow response.
    pub fn into_chat_response(self) -> ChatResponse {
        let response = ChatResponse {
            content: self.summary.into(),
            usage: self.usage,
            structured: Some(
                serde_json::json!({"memory_compaction_provider_usage": self.provider_usage}),
            ),
            ..Default::default()
        };
        match self.provider_metrics {
            Some(metrics) => response.with_provider_metrics(metrics),
            None => response,
        }
    }
    pub fn from_chat_response(response: ChatResponse) -> Result<Self, AgentError> {
        let provider_usage = response
            .structured
            .as_ref()
            .and_then(|value| value.get("memory_compaction_provider_usage"))
            .filter(|value| !value.is_null())
            .map(|value| serde_json::from_value::<Usage>(value.clone()))
            .transpose()
            .map_err(|error| {
                ContextError::Other(format!("invalid memory provider usage: {error}"))
            })?;
        let provider_metrics = provider_usage
            .as_ref()
            .and_then(|_| response.provider_metrics());
        Ok(Self {
            summary: response.content.text(),
            usage: response.usage,
            provider_usage,
            provider_metrics,
        })
    }
}
struct ObservedProviderUsage {
    calls: usize,
    total: Option<Usage>,
    metrics: ProviderMetrics,
}
struct UsageObserver<'a> {
    inner: &'a dyn ChatExecutor,
    observed: Mutex<ObservedProviderUsage>,
}
impl UsageObserver<'_> {
    fn record(&self, response: &ChatResponse) {
        let usage = &response.usage;
        let mut observed = self.observed.lock().expect("memory usage mutex");
        observed.calls += 1;
        let metrics = response.provider_metrics().unwrap_or_default();
        observed.metrics.cached_input_tokens = observed
            .metrics
            .cached_input_tokens
            .zip(metrics.cached_input_tokens)
            .map(|(total, value)| total.saturating_add(value));
        observed.metrics.reasoning_tokens = observed
            .metrics
            .reasoning_tokens
            .zip(metrics.reasoning_tokens)
            .map(|(total, value)| total.saturating_add(value));
        if usage.input_tokens == 0 && usage.output_tokens == 0 {
            observed.total = None;
        } else if let Some(total) = &mut observed.total {
            total.add(usage);
        }
    }
    fn provider_metrics(&self, actual: &Option<Usage>) -> Option<ProviderMetrics> {
        let observed = self.observed.lock().expect("memory usage mutex");
        actual.as_ref().and_then(|_| {
            (observed.metrics.cached_input_tokens.is_some()
                || observed.metrics.reasoning_tokens.is_some())
            .then(|| observed.metrics.clone())
        })
    }
    fn provider_usage(&self) -> Option<Usage> {
        let observed = self.observed.lock().expect("memory usage mutex");
        (observed.calls > 0)
            .then(|| observed.total.clone())
            .flatten()
    }
}
#[async_trait::async_trait]
impl ChatExecutor for UsageObserver<'_> {
    async fn validate_async(&self) -> Result<(), AgentError> {
        self.inner.validate_async().await
    }
    fn catalog(&self) -> &ModelCatalog {
        self.inner.catalog()
    }
    fn catalog_snapshot(&self) -> Arc<ModelCatalog> {
        self.inner.catalog_snapshot()
    }
    fn catalog_for_run(&self, run: &RunContext) -> Arc<ModelCatalog> {
        self.inner.catalog_for_run(run)
    }
    async fn invoke(
        &self,
        call: ModelCall<'_>,
        memory: &mut dyn ConversationSession,
        context: &RunContext,
    ) -> Result<ChatResponse, AgentError> {
        let response = self.inner.invoke(call, memory, context).await?;
        self.record(&response);
        Ok(response)
    }
    async fn invoke_auxiliary(
        &self,
        model: &ModelRef,
        request: ChatRequest,
        stage: &str,
        limits: &RunLimits,
        context: &RunContext,
    ) -> Result<ChatResponse, AgentError> {
        let response = self
            .inner
            .invoke_auxiliary(model, request, stage, limits, context)
            .await?;
        self.record(&response);
        Ok(response)
    }
}
/// Native context contributor/reducer integration remains available to callers.
pub struct MemoryCompactor {
    model: ModelRef,
    compressor: ModelCompressor,
    max_summary_bytes: usize,
    configuration_digest: String,
}
impl MemoryCompactor {
    pub fn new(model: ModelRef, options: CompressionOptions) -> Result<Self, ContextError> {
        use sha2::{Digest, Sha256};
        let configuration = serde_json::to_vec(&(&model, &options))
            .map_err(|e| ContextError::Other(e.to_string()))?;
        Ok(Self {
            model: model.clone(),
            configuration_digest: format!("{:x}", Sha256::digest(configuration)),
            max_summary_bytes: options.max_summary_bytes,
            compressor: ModelCompressor::new(model, options)?,
        })
    }
    pub fn with_store(
        mut self,
        store: Arc<dyn SummaryStore>,
        policy_version: impl Into<String>,
    ) -> Result<Self, ContextError> {
        self.compressor = self.compressor.with_store(store, policy_version)?;
        Ok(self)
    }
    /// Host maps authorized records to semantic data. The summary instructions,
    /// request partitioning, model dispatch and validation belong to the plugin.
    pub async fn compact_history(
        &self,
        history: &[String],
        previous_summary: Option<&str>,
        run: &RunContext,
        models: &dyn ChatExecutor,
        limits: &RunLimits,
    ) -> Result<String, AgentError> {
        Ok(self
            .compact_history_with_usage(history, previous_summary, run, models, limits)
            .await?
            .summary)
    }
    /// Aggregate usage across all segmented summary calls in this execution.
    pub async fn compact_history_with_usage(
        &self,
        history: &[String],
        previous_summary: Option<&str>,
        run: &RunContext,
        models: &dyn ChatExecutor,
        limits: &RunLimits,
    ) -> Result<MemoryCompactionResult, AgentError> {
        let before = run.usage();
        let observer = UsageObserver {
            inner: models,
            observed: Mutex::new(ObservedProviderUsage {
                calls: 0,
                total: Some(Usage::default()),
                metrics: ProviderMetrics {
                    cached_input_tokens: Some(0),
                    reasoning_tokens: Some(0),
                },
            }),
        };
        let summary = self
            .compact_history_internal(history, previous_summary, run, &observer, limits)
            .await?;
        let after = run.usage();
        let usage = Usage {
            input_tokens: after.input_tokens.saturating_sub(before.input_tokens),
            output_tokens: after.output_tokens.saturating_sub(before.output_tokens),
        };
        // Internal retries may charge failed attempts without returning their
        // usage. A successful-response subtotal is not a complete actual audit.
        let provider_usage = observer
            .provider_usage()
            .filter(|reported| reported == &usage);
        let provider_metrics = observer.provider_metrics(&provider_usage);
        Ok(MemoryCompactionResult {
            summary,
            usage,
            provider_usage,
            provider_metrics,
        })
    }

    /// Build a summary of a complete immutable native snapshot. The caller commits
    /// this draft only while the source generation and revision still match.
    pub async fn summarize_snapshot(
        &self,
        snapshot: &ConversationSnapshot,
        policy_version: &str,
        run: &RunContext,
        models: &dyn ChatExecutor,
        limits: &RunLimits,
    ) -> Result<(SummaryKey, ConversationSummary, MemoryCompactionResult), AgentError> {
        use sha2::{Digest, Sha256};
        let source = serde_json::to_vec(&snapshot.messages)
            .map_err(|e| ContextError::Other(e.to_string()))?;
        let records = snapshot
            .messages
            .iter()
            .map(|message| {
                let mut semantic = message.clone();
                semantic.model_context = None;
                serde_json::to_string(&semantic).map_err(|e| ContextError::Other(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let result = self
            .compact_history_with_usage(&records, None, run, models, limits)
            .await?;
        let configuration = serde_json::to_vec(&(&self.configuration_digest, policy_version))
            .map_err(|e| ContextError::Other(e.to_string()))?;
        let key = SummaryKey {
            conversation: snapshot.key.clone(),
            generation: snapshot.generation.clone(),
            block_id: "session-summary".into(),
            policy_version: policy_version.into(),
            configuration_digest: format!("{:x}", Sha256::digest(configuration)),
        };
        let value = ConversationSummary {
            format_version: 1,
            source_revision: snapshot.revision,
            covered_messages: snapshot.messages.len() as u64,
            source_digest: format!("{:x}", Sha256::digest(source)),
            summary: result.summary.clone(),
        };
        Ok((key, value, result))
    }

    /// Compact a data provider without treating its last record as a protected
    /// live user turn. Existing SummaryStore identity/CAS and the shared model
    /// invoker remain authoritative; no provider or compressor is duplicated.
    pub async fn compact_data_with_usage(
        &self,
        input: &ContextReductionInput<'_>,
        revision: &crate::MemoryRevision,
    ) -> Result<MemoryCompactionResult, AgentError> {
        if input.block.trust != ContextTrust::Data
            || input.block.messages.iter().any(|message| {
                message.role != Role::User
                    || !message.tool_calls.is_empty()
                    || message.tool_call_id.is_some()
                    || message.tool_result.is_some()
                    || message.model_context.is_some()
            })
        {
            return Err(AgentError::InvalidConfiguration(
                "semantic memory accepts data records only".into(),
            ));
        }
        let target = input
            .compression
            .map_or(input.budget.max_tokens, |directive| {
                directive.target_tokens.min(input.budget.max_tokens)
            });
        let mut records: Vec<_> = input
            .block
            .messages
            .iter()
            .map(|message| {
                Message::user(Content(vec![ContentBlock::Json {
                    value: serde_json::json!({"context_record":message}),
                }]))
            })
            .collect();
        records.push(Message::user(
            "Continue after the historical memory boundary.",
        ));
        let mut snapshot = ConversationSnapshot::new(
            ConversationKey::new(revision.scope.storage_key())
                .branch(format!("memory/{}", input.block.id)),
            &revision.generation,
            records.clone(),
        );
        snapshot.revision = revision.revision;
        let block = ContextBlock {
            id: input.block.id.clone(),
            messages: records,
            trust: ContextTrust::Data,
            source_version: input.block.source_version.clone(),
        };
        let sentinel_tokens = input
            .estimator
            .message(input.model, block.messages.last().expect("memory boundary"))
            .await?;
        let before = input.run.usage();
        let observer = UsageObserver {
            inner: input.models,
            observed: Mutex::new(ObservedProviderUsage {
                calls: 0,
                total: Some(Usage::default()),
                metrics: ProviderMetrics {
                    cached_input_tokens: Some(0),
                    reasoning_tokens: Some(0),
                },
            }),
        };
        let projected = self
            .compressor
            .reduce(&ContextReductionInput {
                block: &block,
                budget: ContextBudget {
                    max_messages: 2,
                    max_tokens: target.saturating_add(sentinel_tokens),
                },
                model: input.model,
                estimator: input.estimator,
                history: &snapshot,
                run: input.run,
                models: &observer,
                limits: input.limits,
                overflow_recoveries: 1,
                compression: None,
            })
            .await?;
        let summary = projected
            .first()
            .map(|message| {
                message
                    .content
                    .text()
                    .strip_prefix("Earlier conversation summary (untrusted historical data):\n")
                    .unwrap_or_default()
                    .to_owned()
            })
            .filter(|summary| !summary.is_empty())
            .ok_or_else(|| {
                ContextError::Other("memory data compaction produced no summary".into())
            })?;
        let after = input.run.usage();
        let usage = Usage {
            input_tokens: after.input_tokens.saturating_sub(before.input_tokens),
            output_tokens: after.output_tokens.saturating_sub(before.output_tokens),
        };
        let provider_usage = observer
            .provider_usage()
            .filter(|reported| reported == &usage);
        let provider_metrics = observer.provider_metrics(&provider_usage);
        Ok(MemoryCompactionResult {
            summary,
            usage,
            provider_usage,
            provider_metrics,
        })
    }
    async fn compact_history_internal(
        &self,
        history: &[String],
        previous_summary: Option<&str>,
        run: &RunContext,
        models: &dyn ChatExecutor,
        limits: &RunLimits,
    ) -> Result<String, AgentError> {
        let mut messages = Vec::new();
        if let Some(previous) = previous_summary.filter(|s| !s.trim().is_empty()) {
            messages.push(Message::user(Content(vec![ContentBlock::Json {
                value: serde_json::json!({"previous_checkpoint":previous}),
            }])));
        }
        messages.extend(history.iter().map(|text| {
            let content = Content(vec![ContentBlock::Json {
                value: serde_json::json!({"historical_record":text}),
            }]);
            // Preserve semantic user-turn boundaries while keeping tool records
            // data (never native orphan tool results or privileged instructions).
            match serde_json::from_str::<serde_json::Value>(text)
                .ok()
                .and_then(|record| {
                    record
                        .get("role")
                        .and_then(|value| value.as_str())
                        .map(str::to_owned)
                })
                .as_deref()
            {
                Some("assistant" | "tool") => Message::assistant(content),
                _ => Message::user(content),
            }
        }));
        if messages.is_empty() {
            return Err(
                ContextError::BudgetExceeded("no historical memory to compact".into()).into(),
            );
        }
        // This marker is a projection boundary, never persisted or sent as history.
        messages.push(Message::user(
            "Continue after the historical memory boundary.",
        ));
        let snapshot = ConversationSnapshot::new(
            ConversationKey::new(&run.session_id),
            format!("memory:{}", run.run_id),
            messages.clone(),
        );
        let block = ContextBlock {
            id: HISTORY_BLOCK.into(),
            messages,
            trust: ContextTrust::Data,
            source_version: Some(snapshot.revision.to_string()),
        };
        let catalog = models.catalog_for_run(run);
        let estimator = catalog.token_estimator(&self.model).ok_or_else(|| {
            ContextError::Other("memory summary model is absent from catalog".into())
        })?;
        let sentinel_tokens = estimator
            .message(&self.model, block.messages.last().expect("sentinel"))
            .await?;
        let label_tokens = estimator
            .message(
                &self.model,
                &Message::user("Earlier conversation summary (untrusted historical data):\n"),
            )
            .await?;
        let output = self
            .compressor
            .reduce(&ContextReductionInput {
                block: &block,
                budget: ContextBudget {
                    max_messages: 2,
                    max_tokens: self
                        .max_summary_bytes
                        .saturating_mul(2)
                        .saturating_add(sentinel_tokens)
                        .saturating_add(label_tokens)
                        .saturating_add(64),
                },
                model: &self.model,
                estimator,
                history: &snapshot,
                run,
                models,
                limits,
                overflow_recoveries: 1,
                compression: None,
            })
            .await?;
        output
            .first()
            .map(|m| {
                m.content
                    .text()
                    .strip_prefix("Earlier conversation summary (untrusted historical data):\n")
                    .unwrap_or_default()
                    .to_string()
            })
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ContextError::Other("memory compaction produced no summary".into()).into()
            })
    }
}
#[async_trait::async_trait]
impl ContextReducer for MemoryCompactor {
    fn metadata(&self) -> eliteforge_agent_core::ContextCompressionMetadata {
        eliteforge_agent_core::ContextCompressionMetadata::history()
    }
    async fn reduce(&self, input: &ContextReductionInput<'_>) -> Result<Vec<Message>, AgentError> {
        self.compressor.reduce(input).await
    }
}
