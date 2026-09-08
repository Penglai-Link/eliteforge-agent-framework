//! Ordered context blocks with independent quotas. Source history is immutable.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

pub const INSTRUCTIONS_BLOCK: &str = "framework.instructions";
pub const HISTORY_BLOCK: &str = "framework.history";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTrust {
    Instruction,
    Data,
}

#[derive(Clone)]
pub struct ContextBlock {
    pub id: String,
    pub messages: Vec<Message>,
    pub trust: ContextTrust,
    pub source_version: Option<String>,
}
impl ContextBlock {
    pub fn data(id: impl Into<String>, content: impl Into<Content>) -> Self {
        Self {
            id: id.into(),
            messages: vec![Message::user(content)],
            trust: ContextTrust::Data,
            source_version: None,
        }
    }
}
pub struct ContextInput<'a> {
    pub stage: &'a str,
    pub step: usize,
    pub model: &'a ModelRef,
    pub budget: ContextBudget,
    pub current_input: &'a Content,
    pub history: &'a ConversationSnapshot,
    pub estimator: &'a dyn TokenEstimator,
    pub run: &'a RunContext,
}
#[async_trait]
pub trait ContextContributor: Send + Sync {
    fn block_id(&self) -> &str;
    /// Bound upstream queries by the supplied quota and cancellation signal.
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError>;
}

/// A configured data/instruction block without an external query.
pub struct FixedContextContributor(pub ContextBlock);
#[async_trait]
impl ContextContributor for FixedContextContributor {
    fn block_id(&self) -> &str {
        &self.0.id
    }
    async fn contribute(&self, _: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        Ok(self.0.clone())
    }
}

#[derive(Clone, Copy)]
pub struct BlockBudget {
    pub max_tokens: Option<usize>,
    pub max_ratio_bps: Option<u16>,
    pub max_messages: Option<usize>,
}
impl BlockBudget {
    pub fn tokens(tokens: usize) -> Self {
        Self {
            max_tokens: Some(tokens),
            max_ratio_bps: None,
            max_messages: None,
        }
    }
    pub fn ratio(bps: u16) -> Self {
        Self {
            max_tokens: None,
            max_ratio_bps: Some(bps),
            max_messages: None,
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BlockFailure {
    Error,
    Omit,
}
#[derive(Clone)]
pub struct ContextBlockConfig {
    pub id: String,
    pub budget: BlockBudget,
    pub failure: BlockFailure,
    pub reducer: Arc<dyn ContextReducer>,
}
impl ContextBlockConfig {
    pub fn new(id: impl Into<String>, budget: BlockBudget) -> Self {
        Self {
            id: id.into(),
            budget,
            failure: BlockFailure::Error,
            reducer: Arc::new(KeepContext),
        }
    }
    pub fn reduce_with(mut self, reducer: impl ContextReducer + 'static) -> Self {
        self.reducer = Arc::new(reducer);
        self
    }
    pub fn optional(mut self) -> Self {
        self.failure = BlockFailure::Omit;
        self
    }
}
/// Configuration order is prompt order. It is never a retention priority.
#[derive(Clone, Default)]
pub struct ContextLayout(pub Vec<ContextBlockConfig>);
pub trait ContextBudgetPolicy: Send + Sync {
    fn allocate(
        &self,
        blocks: &[ContextBlockConfig],
        available: ContextBudget,
    ) -> Result<Vec<ContextBudget>, AgentError>;
}
pub struct IndependentBlockBudget;
impl ContextBudgetPolicy for IndependentBlockBudget {
    fn allocate(
        &self,
        blocks: &[ContextBlockConfig],
        available: ContextBudget,
    ) -> Result<Vec<ContextBudget>, AgentError> {
        let mut ids = HashSet::new();
        let mut sum = 0usize;
        let mut result = Vec::new();
        for block in blocks {
            if block.id.trim().is_empty()
                || !ids.insert(&block.id)
                || block.budget.max_ratio_bps.is_some_and(|n| n > 10_000)
                || (block.budget.max_tokens.is_none() && block.budget.max_ratio_bps.is_none())
            {
                return Err(AgentError::InvalidConfiguration(
                    "invalid context block budget or ID".into(),
                ));
            }
            let ratio = block
                .budget
                .max_ratio_bps
                .map(|n| ((available.max_tokens as u128 * n as u128) / 10_000) as usize);
            let tokens = block
                .budget
                .max_tokens
                .unwrap_or(usize::MAX)
                .min(ratio.unwrap_or(usize::MAX));
            sum = sum
                .checked_add(tokens)
                .ok_or_else(|| context_budget_error("quota sum overflow"))?;
            result.push(ContextBudget {
                max_tokens: tokens,
                max_messages: block
                    .budget
                    .max_messages
                    .unwrap_or(available.max_messages)
                    .min(available.max_messages),
            });
        }
        if sum > available.max_tokens {
            return Err(context_budget_error("block quotas exceed input capacity"));
        }
        Ok(result)
    }
}

pub struct ContextReductionInput<'a> {
    pub block: &'a ContextBlock,
    pub budget: ContextBudget,
    pub model: &'a ModelRef,
    pub estimator: &'a dyn TokenEstimator,
    pub history: &'a ConversationSnapshot,
    pub run: &'a RunContext,
    pub models: &'a ModelInvoker,
    pub limits: &'a RunLimits,
    pub overflow_recoveries: usize,
}
#[async_trait]
pub trait ContextReducer: Send + Sync {
    async fn reduce(&self, input: &ContextReductionInput<'_>) -> Result<Vec<Message>, AgentError>;
}
pub struct KeepContext;
#[async_trait]
impl ContextReducer for KeepContext {
    async fn reduce(&self, input: &ContextReductionInput<'_>) -> Result<Vec<Message>, AgentError> {
        ensure_context_fits(
            &input.block.messages,
            input.budget,
            input.model,
            input.estimator,
        )?;
        Ok(input.block.messages.clone())
    }
}
pub struct RecentConversation;
#[async_trait]
impl ContextReducer for RecentConversation {
    async fn reduce(&self, input: &ContextReductionInput<'_>) -> Result<Vec<Message>, AgentError> {
        let mut result = input.block.messages.clone();
        trim_context_with(&mut result, input.budget, input.model, input.estimator)?;
        for _ in 0..input.overflow_recoveries {
            if !remove_oldest_turn(&mut result) {
                return Err(context_budget_error(
                    "latest turn cannot be reduced further",
                ));
            }
        }
        Ok(result)
    }
}
pub struct SelectContextEntries;
#[async_trait]
impl ContextReducer for SelectContextEntries {
    async fn reduce(&self, input: &ContextReductionInput<'_>) -> Result<Vec<Message>, AgentError> {
        if input
            .block
            .messages
            .iter()
            .any(|m| m.role != Role::User || !m.tool_calls.is_empty() || m.tool_call_id.is_some())
        {
            return Err(AgentError::InvalidConfiguration(
                "entry selection accepts data messages only".into(),
            ));
        }
        let mut result = Vec::new();
        let mut tokens = 0usize;
        for message in &input.block.messages {
            let size = input.estimator.message(input.model, message);
            if result.len() < input.budget.max_messages
                && tokens.saturating_add(size) <= input.budget.max_tokens
            {
                tokens += size;
                result.push(message.clone());
            }
        }
        Ok(result)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContextBlockOutcome {
    Kept,
    Reduced,
    Omitted { reason: String },
    Failed { reason: String },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBlockReport {
    pub id: String,
    pub position: usize,
    pub source_version: Option<String>,
    pub original_tokens: Option<usize>,
    pub allocated_tokens: usize,
    pub final_tokens: usize,
    pub outcome: ContextBlockOutcome,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextAssemblyReport {
    pub model: ModelRef,
    pub stage: String,
    pub step: usize,
    pub attempt: usize,
    #[serde(default)]
    pub node_execution: Option<WorkflowNodeExecution>,
    pub input_capacity: usize,
    pub protocol_tokens: usize,
    pub estimated_input_tokens: usize,
    pub blocks: Vec<ContextBlockReport>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextFailureReport {
    pub model: ModelRef,
    pub attempt: usize,
    #[serde(default)]
    pub node_execution: Option<WorkflowNodeExecution>,
    pub stage: String,
    pub step: usize,
    pub blocks: Vec<ContextBlockReport>,
    pub error: ErrorReport,
}
impl ContextFailureReport {
    pub fn new(
        model: ModelRef,
        stage: String,
        step: usize,
        attempt: usize,
        node_execution: Option<WorkflowNodeExecution>,
        blocks: Vec<ContextBlockReport>,
        error: &AgentError,
    ) -> Self {
        Self {
            model,
            stage,
            step,
            attempt,
            node_execution,
            blocks,
            error: error.report(&OmitErrorMessages),
        }
    }
}
pub struct ContextWindow {
    pub messages: Vec<Message>,
    pub report: ContextAssemblyReport,
}
pub struct ContextAssemblyInput<'a> {
    pub stage: &'a str,
    pub attempt: usize,
    pub step: usize,
    pub model: &'a ModelRef,
    pub budget: ContextBudget,
    pub estimator: &'a dyn TokenEstimator,
    pub history: &'a ConversationSnapshot,
    pub instructions: &'a [Message],
    pub tools: &'a [ToolDescriptor],
    pub options: &'a GenerationOptions,
    pub run: &'a RunContext,
    pub models: &'a ModelInvoker,
    pub limits: &'a RunLimits,
    pub overflow_recoveries: usize,
}
#[async_trait]
pub trait ContextAssembler: Send + Sync {
    async fn assemble(&self, input: &ContextAssemblyInput<'_>)
    -> Result<ContextWindow, AgentError>;
}
pub struct DefaultContextAssembler {
    contributors: HashMap<String, Arc<dyn ContextContributor>>,
    layout: ContextLayout,
    policy: Arc<dyn ContextBudgetPolicy>,
    history_reducer: Arc<dyn ContextReducer>,
    pub source_timeout: Duration,
    pub max_source_messages: usize,
    pub max_source_bytes: usize,
}
impl Default for DefaultContextAssembler {
    fn default() -> Self {
        Self {
            contributors: HashMap::new(),
            layout: ContextLayout::default(),
            policy: Arc::new(IndependentBlockBudget),
            history_reducer: Arc::new(RecentConversation),
            source_timeout: Duration::from_secs(30),
            max_source_messages: 4096,
            max_source_bytes: 16 * 1024 * 1024,
        }
    }
}
impl DefaultContextAssembler {
    pub fn new(
        layout: ContextLayout,
        contributors: Vec<Arc<dyn ContextContributor>>,
    ) -> Result<Self, AgentError> {
        let mut result = Self {
            layout,
            ..Self::default()
        };
        for contributor in contributors {
            let id = contributor.block_id().to_owned();
            if id.trim().is_empty()
                || is_builtin(&id)
                || result.contributors.insert(id, contributor).is_some()
            {
                return Err(AgentError::InvalidConfiguration(
                    "invalid or duplicate contributor ID".into(),
                ));
            }
        }
        if !result.contributors.is_empty() && result.layout.0.is_empty() {
            return Err(AgentError::InvalidConfiguration(
                "contributors require explicit block quotas and order".into(),
            ));
        }
        if !result.layout.0.is_empty() {
            let mut ids = HashSet::new();
            for block in &result.layout.0 {
                if block.id.trim().is_empty()
                    || !ids.insert(block.id.as_str())
                    || (!is_builtin(&block.id) && !result.contributors.contains_key(&block.id))
                    || (is_builtin(&block.id) && block.failure == BlockFailure::Omit)
                {
                    return Err(AgentError::InvalidConfiguration(
                        "invalid layout source, ID or failure policy".into(),
                    ));
                }
            }
            if !ids.contains(HISTORY_BLOCK)
                || !ids.contains(INSTRUCTIONS_BLOCK)
                || result
                    .contributors
                    .keys()
                    .any(|id| !ids.contains(id.as_str()))
            {
                return Err(AgentError::InvalidConfiguration(
                    "layout must include instructions, history and each contributor exactly once"
                        .into(),
                ));
            }
        }
        Ok(result)
    }
    pub fn with_policy(mut self, policy: Arc<dyn ContextBudgetPolicy>) -> Self {
        self.policy = policy;
        self
    }
    pub fn with_history_reducer(mut self, reducer: Arc<dyn ContextReducer>) -> Self {
        self.history_reducer = reducer;
        self
    }
}

#[async_trait]
impl ContextAssembler for DefaultContextAssembler {
    async fn assemble(
        &self,
        input: &ContextAssemblyInput<'_>,
    ) -> Result<ContextWindow, AgentError> {
        input.run.check_cancelled()?;
        validate_messages(&input.history.messages).map_err(ContextError::from)?;
        let protocol = input
            .estimator
            .tools(input.model, input.tools)
            .saturating_add(input.estimator.options(input.model, input.options));
        let available = ContextBudget {
            max_tokens: input
                .budget
                .max_tokens
                .checked_sub(protocol)
                .ok_or_else(|| context_budget_error("protocol exceeds input capacity"))?,
            max_messages: input.budget.max_messages,
        };
        // Simple Agents have two known sources. Their quotas are fixed once,
        // before reduction. Explicit multi-block layouts always own their quotas.
        let layout = if self.layout.0.is_empty() {
            let fixed = context_tokens(input.instructions, input.model, input.estimator);
            vec![
                ContextBlockConfig::new(
                    INSTRUCTIONS_BLOCK,
                    BlockBudget {
                        max_messages: Some(input.instructions.len()),
                        ..BlockBudget::tokens(fixed)
                    },
                ),
                ContextBlockConfig {
                    id: HISTORY_BLOCK.into(),
                    budget: BlockBudget {
                        max_messages: Some(
                            available
                                .max_messages
                                .saturating_sub(input.instructions.len()),
                        ),
                        ..BlockBudget::tokens(available.max_tokens.checked_sub(fixed).ok_or_else(
                            || context_budget_error("instructions exceed input capacity"),
                        )?)
                    },
                    failure: BlockFailure::Error,
                    reducer: self.history_reducer.clone(),
                },
            ]
        } else {
            self.layout.0.clone()
        };
        let allocations = self.policy.allocate(&layout, available)?;
        if allocations.len() != layout.len()
            || allocations
                .iter()
                .try_fold(0usize, |n, b| n.checked_add(b.max_tokens))
                .is_none_or(|n| n > available.max_tokens)
        {
            return Err(context_budget_error(
                "budget policy returned invalid quotas",
            ));
        }
        let empty = Content::default();
        let current = input
            .history
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map_or(&empty, |m| &m.content);
        let mut messages = Vec::new();
        let mut reports = Vec::new();
        for (position, (config, budget)) in layout.iter().zip(allocations).enumerate() {
            let mut report = ContextBlockReport {
                id: config.id.clone(),
                position,
                source_version: None,
                original_tokens: None,
                allocated_tokens: budget.max_tokens,
                final_tokens: 0,
                outcome: ContextBlockOutcome::Kept,
            };
            let process = async {
                let block = if is_builtin(&config.id) {
                    ContextBlock {
                        id: config.id.clone(),
                        messages: if config.id == HISTORY_BLOCK {
                            input.history.messages.clone()
                        } else {
                            input.instructions.to_vec()
                        },
                        trust: if config.id == INSTRUCTIONS_BLOCK {
                            ContextTrust::Instruction
                        } else {
                            ContextTrust::Data
                        },
                        source_version: (config.id == HISTORY_BLOCK)
                            .then(|| input.history.revision.to_string()),
                    }
                } else {
                    let request = ContextInput {
                        stage: input.stage,
                        step: input.step,
                        model: input.model,
                        budget,
                        current_input: current,
                        history: input.history,
                        estimator: input.estimator,
                        run: input.run,
                    };
                    let block = self.contributors[&config.id].contribute(&request).await?;
                    validate_contribution(&block, self.max_source_messages, self.max_source_bytes)?;
                    block
                };
                if block.id != config.id {
                    return Err(AgentError::InvalidConfiguration(
                        "contributor changed its block ID".into(),
                    ));
                }
                report.source_version = block.source_version.clone();
                report.original_tokens = Some(context_tokens(
                    &block.messages,
                    input.model,
                    input.estimator,
                ));
                let reduction = ContextReductionInput {
                    block: &block,
                    budget,
                    model: input.model,
                    estimator: input.estimator,
                    history: input.history,
                    run: input.run,
                    models: input.models,
                    limits: input.limits,
                    overflow_recoveries: if config.id == HISTORY_BLOCK {
                        input.overflow_recoveries
                    } else {
                        0
                    },
                };
                let output = config.reducer.reduce(&reduction).await?;
                ensure_context_fits(&output, budget, input.model, input.estimator)?;
                validate_messages(&output).map_err(ContextError::from)?;
                if config.id == INSTRUCTIONS_BLOCK && output != input.instructions {
                    return Err(AgentError::InvalidConfiguration(
                        "fixed instructions were changed".into(),
                    ));
                }
                if config.id == HISTORY_BLOCK {
                    validate_history_projection(&block.messages, &output)?;
                }
                if !is_builtin(&config.id) {
                    validate_contribution(
                        &ContextBlock {
                            messages: output.clone(),
                            ..block.clone()
                        },
                        self.max_source_messages,
                        self.max_source_bytes,
                    )?;
                }
                report.final_tokens = context_tokens(&output, input.model, input.estimator);
                if output != block.messages {
                    report.outcome = ContextBlockOutcome::Reduced;
                }
                Ok(output)
            };
            let deadline =
                (tokio::time::Instant::now() + self.source_timeout).min(input.run.deadline);
            let processed = tokio::select! {
                biased;
                _ = input.run.cancelled() => Err(AgentError::Cancelled),
                result = tokio::time::timeout_at(deadline, process) => result.unwrap_or_else(|_| Err(AgentError::Run("context block processing timed out".into()))),
            };
            match processed {
                Ok(output) => messages.extend(output),
                Err(error)
                    if config.failure == BlockFailure::Omit
                        && !matches!(error.primary(), AgentError::Cancelled)
                        && !error.contains_state_error() =>
                {
                    report.outcome = ContextBlockOutcome::Omitted {
                        reason: error.kind().into(),
                    }
                }
                Err(error) => {
                    report.outcome = ContextBlockOutcome::Failed {
                        reason: error.kind().into(),
                    };
                    reports.push(report);
                    let failure = ContextFailureReport::new(
                        input.model.clone(),
                        input.stage.into(),
                        input.step,
                        input.attempt,
                        input.run.node_execution().cloned(),
                        reports,
                        &error,
                    );
                    return Err(AgentError::ContextAssembly {
                        report: Box::new(failure),
                        source: Box::new(error),
                    });
                }
            }
            reports.push(report);
        }
        ensure_context_fits(&messages, available, input.model, input.estimator)?;
        validate_messages(&messages).map_err(ContextError::from)?;
        let total =
            context_tokens(&messages, input.model, input.estimator).saturating_add(protocol);
        Ok(ContextWindow {
            messages,
            report: ContextAssemblyReport {
                model: input.model.clone(),
                stage: input.stage.into(),
                step: input.step,
                attempt: input.attempt,
                node_execution: input.run.node_execution().cloned(),
                input_capacity: input.budget.max_tokens,
                protocol_tokens: protocol,
                estimated_input_tokens: total,
                blocks: reports,
            },
        })
    }
}
fn is_builtin(id: &str) -> bool {
    [INSTRUCTIONS_BLOCK, HISTORY_BLOCK].contains(&id)
}
// Count serialized bytes without allocating another full copy of source data.
struct ByteLimit(usize);
impl std::io::Write for ByteLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("context source exceeds its byte limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn validate_contribution(
    block: &ContextBlock,
    max_messages: usize,
    max_bytes: usize,
) -> Result<(), AgentError> {
    if block.messages.len() > max_messages
        || serde_json::to_writer(ByteLimit(max_bytes), &block.messages).is_err()
    {
        return Err(context_budget_error(
            "source exceeds configured size limits",
        ));
    }
    let role = if block.trust == ContextTrust::Instruction {
        Role::System
    } else {
        Role::User
    };
    if block.messages.iter().any(|m| {
        m.role != role
            || !m.tool_calls.is_empty()
            || m.tool_call_id.is_some()
            || m.tool_result.is_some()
            || m.model_context.is_some()
    }) {
        return Err(AgentError::InvalidConfiguration(
            "context source contains invalid roles or protocol fields".into(),
        ));
    }
    Ok(())
}
fn validate_history_projection(source: &[Message], output: &[Message]) -> Result<(), AgentError> {
    let fixed: Vec<_> = source
        .iter()
        .filter(|m| matches!(m.role, Role::System | Role::Developer))
        .collect();
    let projected: Vec<_> = output
        .iter()
        .filter(|m| matches!(m.role, Role::System | Role::Developer))
        .collect();
    if fixed != projected {
        return Err(AgentError::InvalidConfiguration(
            "reduction changed trusted instructions".into(),
        ));
    }
    if let Some(latest) = source.iter().rposition(|m| m.role == Role::User)
        && !output.ends_with(&source[latest..])
    {
        return Err(AgentError::InvalidConfiguration(
            "reduction changed the latest complete turn".into(),
        ));
    }
    Ok(())
}
pub fn context_tokens(
    messages: &[Message],
    model: &ModelRef,
    estimator: &dyn TokenEstimator,
) -> usize {
    messages
        .iter()
        .fold(0usize, |n, m| n.saturating_add(estimator.message(model, m)))
}
pub fn ensure_context_fits(
    messages: &[Message],
    budget: ContextBudget,
    model: &ModelRef,
    estimator: &dyn TokenEstimator,
) -> Result<(), AgentError> {
    if messages.len() > budget.max_messages
        || context_tokens(messages, model, estimator) > budget.max_tokens
    {
        return Err(context_budget_error("context block exceeds its quota"));
    }
    Ok(())
}
fn context_budget_error(message: &str) -> AgentError {
    ContextError::BudgetExceeded(message.into()).into()
}

#[derive(Debug, Clone, Copy)]
pub struct ContextBudget {
    pub max_messages: usize,
    pub max_tokens: usize,
}

fn over_budget(messages: &[Message], budget: ContextBudget) -> bool {
    messages.len() > budget.max_messages
        || messages
            .iter()
            .map(estimate_message_tokens)
            .fold(0usize, usize::saturating_add)
            > budget.max_tokens
}

pub fn trim_context(
    messages: &mut Vec<Message>,
    budget: ContextBudget,
) -> Result<(), ContextError> {
    while over_budget(messages, budget) {
        if !remove_oldest_turn(messages) {
            return Err(ContextError::BudgetExceeded(
                "fixed instructions and newest turn do not fit; refusing to orphan tool results"
                    .into(),
            ));
        }
    }
    Ok(())
}

pub fn trim_context_with(
    messages: &mut Vec<Message>,
    budget: ContextBudget,
    model: &ModelRef,
    estimator: &dyn TokenEstimator,
) -> Result<(), ContextError> {
    while messages.len() > budget.max_messages
        || messages.iter().fold(0usize, |sum, message| {
            sum.saturating_add(estimator.message(model, message))
        }) > budget.max_tokens
    {
        if !remove_oldest_turn(messages) {
            return Err(ContextError::BudgetExceeded(
                "fixed instructions and newest turn exceed the model-specific context budget"
                    .into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn remove_oldest_turn(messages: &mut Vec<Message>) -> bool {
    let user_indices: Vec<_> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, message)| (message.role == Role::User).then_some(i))
        .collect();
    if user_indices.len() < 2 {
        return false;
    }
    let range = user_indices[0]..user_indices[1];
    let mut index = 0;
    messages.retain(|message| {
        let retain =
            !range.contains(&index) || matches!(message.role, Role::System | Role::Developer);
        index += 1;
        retain
    });
    true
}
