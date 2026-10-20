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
        )
        .await
    }
    async fn project(
        &self,
        input: &ContextReductionInput<'_>,
        persistent: bool,
    ) -> Result<Vec<Message>, AgentError> {
        let messages = &input.block.messages;
        let budget = input.budget;
        let context = input.run;
        validate_messages(messages).map_err(ContextError::from)?;
        context.check_cancelled()?;
        let fits = messages.len() <= budget.max_messages
            && context_tokens(messages, input.model, input.estimator).await? <= budget.max_tokens;
        let newest = messages.iter().rposition(|m| m.role == Role::User);
        let has_completed_turn =
            newest.is_some_and(|index| messages[..index].iter().any(|m| m.role == Role::User));
        // Proactive pressure cannot shrink the protected current turn. Keep a
        // fitting request when there is no completed turn to summarize; actual
        // overflow and configured hard limits still require reduction or fail.
        if fits
            && input.overflow_recoveries == 0
            && (input.compression.is_none() || !has_completed_turn)
        {
            return Ok(messages.clone());
        }
        let newest =
            newest.ok_or_else(|| budget_error("history has no removable completed user turn"))?;
        let mut preserved: Vec<Message> = messages[..newest]
            .iter()
            .filter(|m| m.role == Role::System)
            .cloned()
            .collect();
        let prefix_len = preserved.len();
        preserved.extend_from_slice(&messages[newest..]);
        let reserved = context_tokens(&preserved, input.model, input.estimator)
            .await?
            .saturating_add(
                input
                    .estimator
                    .message(input.model, &Message::user(SUMMARY_LABEL))
                    .await?,
            );
        if preserved.len().saturating_add(1) > budget.max_messages || reserved >= budget.max_tokens
        {
            return Err(budget_error(
                "trusted instructions and newest complete turn leave no summary capacity",
            ));
        }
        let summary_bytes = self.options.max_summary_bytes.min(
            input
                .compression
                .map_or(budget.max_tokens, |d| {
                    d.target_tokens.min(budget.max_tokens)
                })
                .saturating_sub(reserved)
                .max(1),
        ) / input.overflow_recoveries.saturating_add(1);
        if summary_bytes == 0 {
            return Err(budget_error("no summary capacity remains"));
        }
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
                configuration_digest: fingerprint(
                    &json!({"algorithm": "model-compressor-v2", "policy": self.policy_version, "summary_model": self.model, "target_model": input.model, "options": self.options, "tokens": budget.max_tokens, "messages": budget.max_messages, "overflow_recoveries": input.overflow_recoveries, "compression": input.compression}),
                )?,
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
                covered = count;
                summary = record.value.summary.clone();
            }
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
                    summary_bytes,
                    context,
                    &summary_limits,
                )?;
                let capacity = descriptor.input_capacity(&summary_limits, &candidate.options)?;
                if estimator.request(&self.model, &candidate).await? > capacity {
                    break;
                }
                end += 1;
            }
            if end == cursor {
                return Err(budget_error(
                    "one complete historical turn exceeds the summary-model input limit; configure a larger context or offload tool output",
                ));
            }
            let request = summary_request(
                &summary,
                &turns[cursor..end],
                summary_bytes,
                context,
                &summary_limits,
            )?;
            let response = tokio::select! {
                biased;
                _ = context.cancelled() => return Err(AgentError::Cancelled),
                response = input.models.invoke_auxiliary(&self.model, request, "context_compression", &summary_limits, context) => response?,
            };
            summary = parse_summary(response, summary_bytes)?;
            cursor = end;
            calls += 1;
        }
        preserved.insert(
            prefix_len,
            Message::user(format!("{SUMMARY_LABEL}{summary}")),
        );
        validate_messages(&preserved).map_err(ContextError::from)?;
        if (preserved.len() > budget.max_messages
            || context_tokens(&preserved, input.model, input.estimator).await? > budget.max_tokens)
            || (context_tokens(&preserved, input.model, input.estimator).await?
                >= context_tokens(messages, input.model, input.estimator).await?
                && preserved.len() >= messages.len())
        {
            return Err(budget_error(
                "generated summary does not reduce the projection within budget",
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
        Ok(preserved)
    }
}
#[async_trait]
impl ContextReducer for ModelCompressor {
    async fn reduce(&self, input: &ContextReductionInput<'_>) -> Result<Vec<Message>, AgentError> {
        self.project(input, true).await
    }
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

fn summary_request(
    previous: &str,
    history: &[Value],
    max_bytes: usize,
    context: &RunContext,
    limits: &RunLimits,
) -> Result<ChatRequest, AgentError> {
    Ok(ChatRequest {
        messages: vec![
            Message::system(ManagedDeveloperPrompts::resolve(context, "memory.context-summary", SUMMARY_PROMPT.trim())?)
                .with_instruction_origin(InstructionOrigin::Plugin),
            Message::user(Content(vec![ContentBlock::Json {
                value: json!({
                    "previous_summary": previous, "history": history, "summary_max_utf8_bytes": max_bytes,
                }),
            }])),
        ],
        tools: vec![],
        extensions: context.extensions.clone(),
        options: GenerationOptions {
            temperature: Some(0.0),
            top_p: Some(1.0),
            // Bytes conservatively bound tokens; validation below enforces bytes
            // regardless of tokenization or provider conformance.
            max_output_tokens: Some(
                max_bytes
                    .saturating_add(32)
                    .min(limits.max_model_output_tokens as usize) as u32,
            ),
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

fn parse_summary(response: ChatResponse, max_bytes: usize) -> Result<String, ContextError> {
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
    if result.summary.len() > max_bytes {
        return Err(ContextError::BudgetExceeded(
            "summary model exceeded requested UTF-8 byte budget".into(),
        ));
    }
    Ok(result.summary)
}
