//! ChatModel-backed, fail-closed reduction of a model-input projection.
//!
//! The input is borrowed: the durable transcript is never replaced. Trusted
//! instructions and the complete latest user turn (including tools and opaque
//! model continuation) survive byte-for-byte. Historical data is serialized as
//! data for a dedicated summary model, never promoted to system instructions.
use async_trait::async_trait;
use eliteforge_agent_core::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const SUMMARY_LABEL: &str = "Earlier conversation summary (untrusted historical data):\n";
const SUMMARY_PROMPT: &str = include_str!("../prompts/context-summary.txt");

#[derive(Debug, Clone, Serialize)]
pub struct CompressionOptions {
    /// Hard input cap in the summary connection's token estimate.
    pub max_input_tokens: usize,
    /// Independent UTF-8 storage/safety cap; never interpreted as a token budget.
    pub max_summary_bytes: usize,
    pub max_model_calls: usize,
}
impl Default for CompressionOptions {
    fn default() -> Self {
        Self {
            max_input_tokens: 64_000,
            max_summary_bytes: 4_000,
            max_model_calls: 16,
        }
    }
}

pub struct ModelCompressor {
    model: ModelRef,
    options: CompressionOptions,
    store: Option<std::sync::Arc<dyn crate::SummaryStore>>,
    policy_version: String,
}
impl ModelCompressor {
    pub fn new(model: ModelRef, options: CompressionOptions) -> Result<Self, ContextError> {
        if options.max_input_tokens == 0
            || options.max_summary_bytes == 0
            || options.max_model_calls == 0
        {
            return Err(ContextError::Other(
                "compression limits must be positive".into(),
            ));
        }
        Ok(Self {
            model,
            options,
            store: None,
            policy_version: "model-compressor-v1".into(),
        })
    }
    /// Change the version when changing summary semantics or the configured model
    /// behind the same model reference. Old derived summaries are not reused.
    pub fn with_store(
        mut self,
        store: std::sync::Arc<dyn crate::SummaryStore>,
        policy_version: impl Into<String>,
    ) -> Result<Self, ContextError> {
        let version = policy_version.into();
        if version.trim().is_empty() || version.len() > 256 {
            return Err(ContextError::Other(
                "summary policy version must contain 1..=256 bytes".into(),
            ));
        }
        self.store = Some(store);
        self.policy_version = version;
        Ok(self)
    }
    /// One-off projection without persistent identity. Use ContextReducer to
    /// enable summary reuse against a real ConversationSnapshot.
    pub async fn compress(
        &self,
        messages: &[Message],
        budget: ContextBudget,
        context: &RunContext,
        models: &dyn ChatExecutor,
        limits: &RunLimits,
    ) -> Result<Vec<Message>, AgentError> {
        let snapshot = ConversationSnapshot::new(
            ConversationKey::new(&context.session_id),
            uuid::Uuid::new_v4().to_string(),
            messages.to_vec(),
        );
        let block = ContextBlock {
            id: HISTORY_BLOCK.into(),
            messages: messages.to_vec(),
            trust: ContextTrust::Data,
            source_version: None,
        };
        self.project(
            &ContextReductionInput {
                block: &block,
                budget,
                model: &self.model,
                estimator: &ConservativeTokenEstimator,
                history: &snapshot,
                run: context,
                models,
                limits,
                overflow_recoveries: 0,
                compression: None,
            },
            false,
            None,
        )
        .await
        .map(HistoryProjection::into_messages)
    }
    async fn project(
        &self,
        input: &ContextReductionInput<'_>,
        persistent: bool,
        split: Option<(ContextBudget, usize)>,
    ) -> Result<HistoryProjection, AgentError> {
        let messages = &input.block.messages;
        let budget = input.budget;
        let context = input.run;
        validate_messages(messages).map_err(ContextError::from)?;
        context.check_cancelled()?;
        if persistent && input.compression.is_none() && input.overflow_recoveries == 0 {
            if let Some(projection) = self.load_projection(input, split).await? {
                return Ok(projection);
            }
        }
        let fits = messages.len() <= budget.max_messages
            && context_tokens(messages, input.model, input.estimator).await? <= budget.max_tokens;
        let users = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User)
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        let mut newest = if let Some((_, turns)) = split {
            // Maintenance requests and failed prompts may have no assistant
            // response. They stay in the raw suffix, but must not evict the
            // most recent actual conversation/tool turn from retention.
            let completed: Vec<_> = users
                .iter()
                .enumerate()
                .filter_map(|(position, start)| {
                    let end = users.get(position + 1).copied().unwrap_or(messages.len());
                    (position + 1 == users.len()
                        || messages[*start..end]
                            .iter()
                            .any(|m| m.role == Role::Assistant))
                    .then_some(*start)
                })
                .collect();
            completed
                .get(completed.len().saturating_sub(turns))
                .copied()
        } else {
            users.last().copied()
        };
        // Retain whole recent turns; if necessary drop the oldest retained turn,
        // never the current turn or a tool result by itself.
        if split.is_some() {
            while let Some(start) = newest {
                let mut retained = messages[..start]
                    .iter()
                    .filter(|m| m.role == Role::System)
                    .cloned()
                    .collect::<Vec<_>>();
                retained.extend_from_slice(&messages[start..]);
                if context_tokens(&retained, input.model, input.estimator).await?
                    <= budget.max_tokens
                {
                    break;
                }
                let Some(next) = users.iter().copied().find(|index| *index > start) else {
                    break;
                };
                newest = Some(next);
            }
        }
        let has_completed_turn =
            newest.is_some_and(|index| messages[..index].iter().any(|m| m.role == Role::User));
        // Proactive pressure cannot shrink the protected current turn. Keep a
        // fitting request when there is no completed turn to summarize; actual
        // overflow and configured hard limits still require reduction or fail.
        if fits
            && input.overflow_recoveries == 0
            && (!has_completed_turn
                || (input.compression.is_none() && (!persistent || self.store.is_none())))
        {
            return Ok(HistoryProjection {
                summary: vec![],
                recent: messages.clone(),
            });
        }
        let newest =
            newest.ok_or_else(|| budget_error("history has no removable completed user turn"))?;
        let mut preserved: Vec<Message> = messages[..newest]
            .iter()
            .filter(|m| m.role == Role::System)
            .cloned()
            .collect();
        preserved.extend_from_slice(&messages[newest..]);
        let reserved = context_tokens(&preserved, input.model, input.estimator)
            .await?
            .saturating_add(
                input
                    .estimator
                    .message(input.model, &Message::user(SUMMARY_LABEL))
                    .await?,
            );
        if preserved.len().saturating_add(usize::from(split.is_none())) > budget.max_messages
            || (if split.is_some() {
                context_tokens(&preserved, input.model, input.estimator).await? > budget.max_tokens
            } else {
                reserved >= budget.max_tokens
            })
        {
            if fits && input.compression.is_none() {
                return Ok(HistoryProjection {
                    summary: vec![],
                    recent: messages.clone(),
                });
            }
            return Err(budget_error(
                "trusted instructions and newest complete turn leave no summary capacity",
            ));
        }
        let summary_tokens = if let Some((summary_budget, _)) = split {
            let label = input
                .estimator
                .message(input.model, &Message::user(SUMMARY_LABEL))
                .await?;
            input
                .compression
                .map_or(summary_budget.max_tokens, |d| {
                    d.target_tokens.min(summary_budget.max_tokens)
                })
                .checked_sub(label)
                .ok_or_else(|| budget_error("summary block leaves no content capacity"))?
        } else {
            input
                .compression
                .map_or(budget.max_tokens, |d| {
                    d.target_tokens.min(budget.max_tokens)
                })
                .saturating_sub(reserved)
                .max(1)
                / input.overflow_recoveries.saturating_add(1)
        };
        // A token-budget-derived byte guard scales with the summary block. It is
        // deliberately generous; the actual target-model tokenizer admits text.
        let summary_bytes = split.map_or(self.options.max_summary_bytes, |(budget, _)| {
            summary_byte_guard(budget.max_tokens)
        });
        if summary_tokens == 0 {
            return Err(budget_error("no summary capacity remains"));
        }
        let label_tokens = input
            .estimator
            .message(input.model, &Message::user(SUMMARY_LABEL))
            .await?;
        let cache_key = if persistent && self.store.is_some() {
            if messages != &input.history.messages {
                return Err(ContextError::Other(
                    "persistent history summaries require the complete conversation snapshot"
                        .into(),
                )
                .into());
            }
            Some(crate::SummaryKey {
                conversation: input.history.key.clone(),
                generation: input.history.generation.clone(),
                block_id: input.block.id.clone(),
                policy_version: self.policy_version.clone(),
                configuration_digest: self.projection_digest(input.model, split)?,
            })
        } else {
            None
        };
        let loaded = match (&cache_key, &self.store) {
            (Some(key), Some(store)) => store.load(key).await?,
            _ => None,
        };
        let mut covered = 0;
        let mut summary = String::new();
        if let Some(record) = &loaded {
            let count = usize::try_from(record.value.covered_messages).unwrap_or(usize::MAX);
            if record.value.format_version == 1
                && count > 0
                && count <= newest
                && messages[count].role == Role::User
                && record.value.source_revision <= input.history.revision
                && record.value.summary.len() <= summary_bytes
                && record.value.source_digest == fingerprint(&messages[..count])?
            {
                let tokens = input
                    .estimator
                    .message(
                        input.model,
                        &Message::user(format!("{SUMMARY_LABEL}{}", record.value.summary)),
                    )
                    .await?
                    .saturating_sub(label_tokens);
                if tokens <= summary_tokens {
                    covered = count;
                    summary = record.value.summary.clone();
                }
            }
        }
        if fits && input.compression.is_none() && input.overflow_recoveries == 0 {
            if covered == 0 {
                return Ok(HistoryProjection {
                    summary: vec![],
                    recent: messages.clone(),
                });
            }
            let mut recent: Vec<_> = messages[..covered]
                .iter()
                .filter(|m| m.role == Role::System)
                .cloned()
                .collect();
            recent.extend_from_slice(&messages[covered..]);
            let projection = HistoryProjection {
                summary: vec![Message::user(format!("{SUMMARY_LABEL}{summary}"))],
                recent,
            };
            if self.projection_fits(&projection, input, split).await? {
                return Ok(projection);
            }
            return Ok(HistoryProjection {
                summary: vec![],
                recent: messages.clone(),
            });
        }
        let turns = history_turns(&messages[covered..newest]);
        if turns.is_empty() && summary.is_empty() {
            return Err(budget_error(
                "no historical conversation data can be summarized",
            ));
        }
        let catalog = input.models.catalog_for_run(input.run);
        let descriptor = catalog.get(&self.model).ok_or_else(|| {
            ContextError::Other("summary model is absent from the model catalog".into())
        })?;
        let estimator = catalog
            .token_estimator(&self.model)
            .expect("validated summary model");
        let mut summary_limits = input.limits.clone();
        summary_limits.max_context_tokens = self
            .options
            .max_input_tokens
            .min(input.limits.max_context_tokens);
        let mut cursor = 0;
        let mut calls = 0;
        while cursor < turns.len() {
            context.check_cancelled()?;
            if calls >= self.options.max_model_calls {
                return Err(budget_error(
                    "history exceeds configured summary-call limit",
                ));
            }
            let mut end = cursor;
            while end < turns.len() {
                let candidate = summary_request(
                    &summary,
                    &turns[cursor..=end],
                    SummaryBudget::new(summary_tokens, summary_bytes),
                    None,
                    context,
                    &summary_limits,
                )?;
                let capacity = descriptor.input_capacity(&summary_limits, &candidate.options)?;
                let cost = estimator.request(&self.model, &candidate).await?;
                if cost > capacity {
                    break;
                }
                // Reserve feedback space when packing additional turns, without
                // rejecting a complete turn that already fits on its own.
                if end > cursor && cost.saturating_add((capacity / 8).min(512)) > capacity {
                    break;
                }
                end += 1;
            }
            if end == cursor {
                return Err(budget_error(
                    "one complete historical turn exceeds the summary-model input limit; configure a larger context or offload tool output",
                ));
            }
            let mut previous_attempt = None;
            let mut summary_budget = SummaryBudget::new(summary_tokens, summary_bytes);
            for attempt in 0..3 {
                context.check_cancelled()?;
                if calls >= self.options.max_model_calls {
                    return Err(budget_error(
                        "history exceeds configured summary-call limit",
                    ));
                }
                let mut request = summary_request(
                    &summary,
                    &turns[cursor..end],
                    summary_budget,
                    previous_attempt.as_ref(),
                    context,
                    &summary_limits,
                )?;
                let capacity = descriptor.input_capacity(&summary_limits, &request.options)?;
                if estimator.request(&self.model, &request).await? > capacity {
                    // A very large draft need not be echoed back. Regenerate from
                    // the unchanged evidence with measured feedback instead.
                    if let Some(feedback) = previous_attempt.as_mut() {
                        feedback
                            .as_object_mut()
                            .expect("framework retry metadata")
                            .remove("summary");
                        request = summary_request(
                            &summary,
                            &turns[cursor..end],
                            summary_budget,
                            Some(feedback),
                            context,
                            &summary_limits,
                        )?;
                    }
                    if estimator.request(&self.model, &request).await? > capacity {
                        return Err(budget_error(
                            "summary retry exceeds the summary-model input limit",
                        ));
                    }
                }
                let response = tokio::select! {
                    biased;
                    _ = context.cancelled() => return Err(AgentError::Cancelled),
                    response = input.models.invoke_auxiliary(&self.model, request, "context_compression", &summary_limits, context) => response,
                };
                calls += 1;
                let candidate = match response {
                    Ok(response) => parse_summary(response)?,
                    Err(error)
                        if matches!(
                            error.primary(),
                            AgentError::Model(ModelError::InvalidOutput(_))
                        ) && attempt < 2 =>
                    {
                        // A known malformed structured result is safe to regenerate:
                        // auxiliary summaries have no tools or external effects.
                        // Keep strict schema validation and never persist the draft.
                        previous_attempt = Some(
                            json!({"format_error":"The previous response was not valid structured JSON. Return exactly one JSON object with one nonempty string field named summary; no markdown fences or prose outside it.", "instruction":"Regenerate from the unchanged original history and obey both limits."}),
                        );
                        summary_budget.target_characters =
                            (summary_budget.target_characters * 3 / 4).max(1);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let actual_tokens = input
                    .estimator
                    .message(
                        input.model,
                        &Message::user(format!("{SUMMARY_LABEL}{candidate}")),
                    )
                    .await?
                    .saturating_sub(label_tokens);
                if candidate.len() <= summary_bytes && actual_tokens <= summary_tokens {
                    summary = candidate;
                    break;
                }
                if attempt == 2 {
                    return Err(budget_error(
                        "summary remains over its token or UTF-8 byte budget after 3 attempts",
                    ));
                }
                // Measured feedback narrows the next request. Do not truncate facts
                // or publish a partial summary if retries cannot satisfy both caps.
                summary_budget.target_characters = summary_budget
                    .target_characters
                    .min(
                        candidate
                            .chars()
                            .count()
                            .saturating_mul(summary_tokens)
                            .checked_div(actual_tokens.max(1))
                            .unwrap_or(0),
                    )
                    .min(
                        candidate
                            .chars()
                            .count()
                            .saturating_mul(summary_bytes)
                            .checked_div(candidate.len().max(1))
                            .unwrap_or(0),
                    )
                    .saturating_mul(3)
                    .checked_div(4)
                    .unwrap_or(0)
                    .max(1);
                previous_attempt = Some(json!({
                    "summary": candidate,
                    "actual_tokens": actual_tokens,
                    "actual_utf8_bytes": candidate.len(),
                    "instruction": "Compress this overlong draft again using the original history as evidence. Obey both independent limits and the reduced character target."
                }));
            }
            cursor = end;
        }
        let projected = HistoryProjection {
            summary: vec![Message::user(format!("{SUMMARY_LABEL}{summary}"))],
            recent: preserved,
        };
        validate_messages(&projected.clone().into_messages()).map_err(ContextError::from)?;
        if !self.projection_fits(&projected, input, split).await? {
            return Err(budget_error(
                "generated summary does not fit the independent history and summary quotas",
            ));
        }
        context.check_cancelled()?;
        if calls > 0
            && let (Some(key), Some(store)) = (&cache_key, &self.store)
        {
            let record = crate::ConversationSummary {
                format_version: 1,
                source_revision: input.history.revision,
                covered_messages: newest as u64,
                source_digest: fingerprint(&messages[..newest])?,
                summary,
            };
            // A concurrent writer can win. Its record remains intact; this valid
            // local projection is still usable for the snapshot we assembled.
            store
                .compare_exchange(key, loaded.as_ref().map(|r| r.version.as_str()), &record)
                .await?;
        }
        Ok(projected)
    }

    fn configuration_digest(&self, target_model: &ModelRef) -> Result<String, ContextError> {
        fingerprint(
            &json!({"algorithm":"model-compressor-v7", "policy":self.policy_version,
            "summary_model":self.model, "target_model":target_model, "options":self.options}),
        )
    }
}
#[async_trait]
impl ContextReducer for ModelCompressor {
    async fn project_history(
        &self,
        input: &ContextReductionInput<'_>,
        summary_budget: ContextBudget,
        recent_turns: usize,
        cached_only: bool,
    ) -> Result<Option<HistoryProjection>, AgentError> {
        let split = Some((summary_budget, recent_turns));
        if cached_only {
            self.load_projection(input, split).await
        } else {
            self.project(input, true, split).await.map(Some)
        }
    }
    async fn cached_projection(
        &self,
        input: &ContextReductionInput<'_>,
    ) -> Result<Option<Vec<Message>>, AgentError> {
        Ok(self
            .load_projection(input, None)
            .await?
            .map(HistoryProjection::into_messages))
    }
    async fn reduce(&self, input: &ContextReductionInput<'_>) -> Result<Vec<Message>, AgentError> {
        self.project(input, true, None)
            .await
            .map(HistoryProjection::into_messages)
    }
}
impl ModelCompressor {
    fn projection_digest(
        &self,
        model: &ModelRef,
        split: Option<(ContextBudget, usize)>,
    ) -> Result<String, ContextError> {
        fingerprint(&(
            self.configuration_digest(model)?,
            split.map(|(budget, turns)| (budget.max_tokens, budget.max_messages, turns)),
        ))
    }
    async fn projection_fits(
        &self,
        projected: &HistoryProjection,
        input: &ContextReductionInput<'_>,
        split: Option<(ContextBudget, usize)>,
    ) -> Result<bool, AgentError> {
        if let Some((summary, _)) = split {
            Ok(projected.summary.len() <= summary.max_messages
                && projected.recent.len() <= input.budget.max_messages
                && context_tokens(&projected.summary, input.model, input.estimator).await?
                    <= summary.max_tokens
                && context_tokens(&projected.recent, input.model, input.estimator).await?
                    <= input.budget.max_tokens)
        } else {
            let messages = projected.clone().into_messages();
            Ok(messages.len() <= input.budget.max_messages
                && context_tokens(&messages, input.model, input.estimator).await?
                    <= input.budget.max_tokens)
        }
    }
    async fn load_projection(
        &self,
        input: &ContextReductionInput<'_>,
        split: Option<(ContextBudget, usize)>,
    ) -> Result<Option<HistoryProjection>, AgentError> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        let messages = &input.block.messages;
        if messages != &input.history.messages {
            return Ok(None);
        }
        let Some(newest) = messages.iter().rposition(|m| m.role == Role::User) else {
            return Ok(None);
        };
        let key = crate::SummaryKey {
            conversation: input.history.key.clone(),
            generation: input.history.generation.clone(),
            block_id: input.block.id.clone(),
            policy_version: self.policy_version.clone(),
            configuration_digest: self.projection_digest(input.model, split)?,
        };
        let Some(record) = store.load(&key).await? else {
            return Ok(None);
        };
        let count = usize::try_from(record.value.covered_messages).unwrap_or(usize::MAX);
        let max_bytes = split.map_or(self.options.max_summary_bytes, |(budget, _)| {
            summary_byte_guard(budget.max_tokens)
        });
        if record.value.format_version != 1
            || count == 0
            || count > newest
            || messages[count].role != Role::User
            || record.value.source_revision > input.history.revision
            || record.value.summary.len() > max_bytes
            || record.value.source_digest != fingerprint(&messages[..count])?
        {
            return Ok(None);
        }
        let mut recent: Vec<_> = messages[..count]
            .iter()
            .filter(|m| m.role == Role::System)
            .cloned()
            .collect();
        recent.extend_from_slice(&messages[count..]);
        let projection = HistoryProjection {
            summary: vec![Message::user(format!(
                "{SUMMARY_LABEL}{}",
                record.value.summary
            ))],
            recent,
        };
        validate_messages(&projection.clone().into_messages()).map_err(ContextError::from)?;
        if !self.projection_fits(&projection, input, split).await? {
            return Ok(None);
        }
        Ok(Some(projection))
    }
}
/// A framing guard derived from token capacity, not a fixed summary quota.
pub fn summary_byte_guard(tokens: usize) -> usize {
    tokens.saturating_mul(16).max(1024)
}

fn budget_error(message: &str) -> AgentError {
    ContextError::BudgetExceeded(message.into()).into()
}

// Stream serialization into the digest, avoiding an extra full history buffer.
fn fingerprint<T: Serialize + ?Sized>(value: &T) -> Result<String, ContextError> {
    use sha2::Digest;
    struct HashWriter(sha2::Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(sha2::Sha256::new());
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| ContextError::Other("summary source cannot be fingerprinted".into()))?;
    Ok(format!("{:x}", writer.0.finalize()))
}

// Only semantic history is sent as data. Provider-owned signatures are neither
// transferred to a different model nor exposed as text to the summary model.
#[derive(Serialize)]
struct HistoricalMessage<'a> {
    role: Role,
    content: &'a Content,
    tool_calls: &'a [ToolCall],
    tool_call_id: &'a Option<String>,
    tool_result: &'a Option<ToolResultMetadata>,
    structured: &'a Option<Value>,
}

fn history_turns(messages: &[Message]) -> Vec<Value> {
    let mut turns = Vec::new();
    let mut turn = Vec::new();
    for message in messages {
        if message.role == Role::System {
            continue;
        }
        if message.role == Role::User && !turn.is_empty() {
            turns.push(Value::Array(std::mem::take(&mut turn)));
        }
        turn.push(
            serde_json::to_value(HistoricalMessage {
                role: message.role,
                content: &message.content,
                tool_calls: &message.tool_calls,
                tool_call_id: &message.tool_call_id,
                tool_result: &message.tool_result,
                structured: &message.structured,
            })
            .expect("framework message content is serializable"),
        );
    }
    if !turn.is_empty() {
        turns.push(Value::Array(turn));
    }
    turns
}

#[derive(Clone, Copy)]
struct SummaryBudget {
    max_tokens: usize,
    max_bytes: usize,
    target_characters: usize,
}
impl SummaryBudget {
    fn new(max_tokens: usize, max_bytes: usize) -> Self {
        Self {
            max_tokens,
            max_bytes,
            // Advisory, with headroom for CJK tokenization and UTF-8 width.
            // Admission still uses the target connection's token estimator.
            target_characters: (max_tokens / 2)
                .min(max_bytes / 4)
                .saturating_mul(4)
                .checked_div(5)
                .unwrap_or(0)
                .max(1),
        }
    }
}

fn summary_request(
    previous: &str,
    history: &[Value],
    budget: SummaryBudget,
    retry: Option<&Value>,
    context: &RunContext,
    limits: &RunLimits,
) -> Result<ChatRequest, AgentError> {
    Ok(ChatRequest {
        messages: vec![
            Message::system(ManagedDeveloperPrompts::resolve(
                context,
                "memory.context-summary",
                SUMMARY_PROMPT.trim(),
            )?)
            .with_instruction_origin(InstructionOrigin::Plugin),
            Message::user(Content(vec![ContentBlock::Json {
                value: json!({
                    "previous_summary": previous, "history": history,
                    "summary_max_tokens": budget.max_tokens,
                    "summary_max_utf8_bytes": budget.max_bytes,
                    "summary_target_characters": budget.target_characters,
                    "overlong_draft": retry,
                }),
            }])),
        ],
        tools: vec![],
        extensions: context.extensions.clone(),
        options: GenerationOptions {
            temperature: Some(0.0),
            top_p: Some(1.0),
            // The configured generation budget also covers provider reasoning.
            // Summary bytes are independently enforced after parsing the JSON.
            max_output_tokens: Some(limits.max_model_output_tokens),
            output_schema: Some(OutputSchema {
                name: "conversation_summary".into(),
                schema: json!({
                    "type":"object", "properties":{"summary":{"type":"string","minLength":1}},
                    "required":["summary"], "additionalProperties":false,
                }),
            }),
            ..GenerationOptions::default()
        },
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SummaryResponse {
    summary: String,
}

fn parse_summary(response: ChatResponse) -> Result<String, ContextError> {
    if !response.tool_calls.is_empty() {
        return Err(ContextError::Other(
            "summary model unexpectedly requested tools".into(),
        ));
    }
    let value = if let Some(value) = response.structured {
        value
    } else {
        match response.content.0.as_slice() {
            [ContentBlock::Json { value }] => value.clone(),
            blocks
                if blocks
                    .iter()
                    .all(|b| matches!(b, ContentBlock::Text { .. })) =>
            {
                serde_json::from_str(&response.content.text()).map_err(|_| {
                    ContextError::Other("summary model returned invalid JSON".into())
                })?
            }
            _ => {
                return Err(ContextError::Other(
                    "summary model returned unsupported mixed content".into(),
                ));
            }
        }
    };
    let result: SummaryResponse = serde_json::from_value(value).map_err(|_| {
        ContextError::Other("summary model returned an invalid summary schema".into())
    })?;
    if result.summary.trim().is_empty() {
        return Err(ContextError::Other(
            "summary model returned an empty summary".into(),
        ));
    }
    Ok(result.summary)
}
