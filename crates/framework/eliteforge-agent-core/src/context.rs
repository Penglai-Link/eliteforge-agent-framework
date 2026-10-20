//! Ordered context blocks with independent quotas. Source history is immutable.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

pub const INSTRUCTIONS_BLOCK: &str = "framework.instructions";
pub const HISTORY_BLOCK: &str = "framework.history";

/// Host-managed developer instructions resolved for one run. A host that
/// supplies this extension owns every listed prompt and may disable one by
/// omitting it; framework defaults are used only by standalone hosts.
#[derive(Clone, Default)]
pub struct ManagedDeveloperPrompts(pub BTreeMap<String, String>);

impl ManagedDeveloperPrompts {
    pub fn resolve(run: &RunContext, key: &str, standalone_default: &str) -> Result<String, AgentError> {
        match run.extensions.get::<Self>() {
            Some(prompts) => prompts.0.get(key).cloned().ok_or_else(|| {
                AgentError::InvalidConfiguration(format!("managed developer prompt is unavailable: {key}"))
            }),
            None => Ok(standalone_default.to_owned()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextTrust {
    Instruction,
    Data,
    /// A fixed instruction followed by explicitly lower-trust user data.
    Mixed,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ContextBlock {
    pub id: String,
    pub messages: Vec<Message>,
    pub trust: ContextTrust,
    pub source_version: Option<String>,
}
impl ContextBlock {
    pub fn instructions(id: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            id: id.into(),
            messages,
            trust: ContextTrust::Instruction,
            source_version: None,
        }
    }
    pub fn data(id: impl Into<String>, content: impl Into<Content>) -> Self {
        Self {
            id: id.into(),
            messages: vec![Message::user(content)],
            trust: ContextTrust::Data,
            source_version: None,
        }
    }
    pub fn instructions_with_data(id: impl Into<String>, messages: Vec<Message>) -> Self {
        Self { id: id.into(), messages, trust: ContextTrust::Mixed, source_version: None }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ContextSourceKind {
    Instruction,
    History,
    #[default]
    Data,
}

#[crate::extension_point(adapter = crate::ContextContributorPlugin, contribute)]
#[async_trait]
pub trait ContextContributor: Send + Sync {
    /// True when this source loads and contributes explicit user-selected skills.
    fn supports_requested_skills(&self) -> bool {
        false
    }

    fn kind(&self) -> ContextSourceKind {
        ContextSourceKind::Data
    }
    fn block_id(&self) -> &str;
    /// Bound upstream queries by the supplied quota and cancellation signal.
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError>;
}

/// A configured data/instruction block without an external query.
pub struct FixedContextContributor(pub ContextBlock);
#[async_trait]
impl ContextContributor for FixedContextContributor {
    fn kind(&self) -> ContextSourceKind {
        if self.0.trust != ContextTrust::Data {
            ContextSourceKind::Instruction
        } else {
            ContextSourceKind::Data
        }
    }
    fn block_id(&self) -> &str {
        &self.0.id
    }
    async fn contribute(&self, _: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        Ok(self.0.clone())
    }
}

pub struct FixedInstructions(FixedContextContributor);
impl FixedInstructions {
    pub fn new(id: impl Into<String>, instructions: impl Into<String>) -> Self {
        Self(FixedContextContributor(ContextBlock::instructions(
            id,
            vec![Message::system(instructions.into())],
        )))
    }
    pub fn messages(id: impl Into<String>, messages: Vec<Message>) -> Self {
        Self(FixedContextContributor(ContextBlock::instructions(
            id, messages,
        )))
    }
}
#[async_trait]
impl ContextContributor for FixedInstructions {
    fn kind(&self) -> ContextSourceKind {
        ContextSourceKind::Instruction
    }
    fn block_id(&self) -> &str {
        self.0.block_id()
    }
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        self.0.contribute(input).await
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
#[derive(Debug, Clone, Copy)]
pub struct ContextCompressionPolicy {
    pub trigger_ratio_bps: u16,
    pub target_ratio_bps: u16,
    pub min_gain_tokens: usize,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextCompressionTrigger {
    #[default]
    None,
    Block,
    Global,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompressionDirective {
    pub trigger: ContextCompressionTrigger,
    /// A soft reduction target; the block allocation remains the hard limit.
    pub target_tokens: usize,
}
#[derive(Clone)]
pub struct ContextBlockConfig {
    pub id: String,
    pub budget: BlockBudget,
    pub failure: BlockFailure,
    pub reducer: Arc<dyn ContextReducer>,
    pub compression: Option<ContextCompressionPolicy>,
}
impl ContextBlockConfig {
    pub fn new(id: impl Into<String>, budget: BlockBudget) -> Self {
        Self {
            id: id.into(),
            budget,
            failure: BlockFailure::Error,
            reducer: Arc::new(KeepContext),
            compression: None,
        }
    }
    pub fn reduce_with(mut self, reducer: impl ContextReducer + 'static) -> Self {
        self.reducer = Arc::new(reducer);
        self
    }
    pub fn compress_at(mut self, trigger_ratio_bps: u16, target_ratio_bps: u16) -> Self {
        self.compression = Some(ContextCompressionPolicy {
            trigger_ratio_bps,
            target_ratio_bps,
            min_gain_tokens: 1,
        });
        self
    }
    pub fn with_min_compression_gain(mut self, tokens: usize) -> Self {
        if let Some(policy) = &mut self.compression {
            policy.min_gain_tokens = tokens.max(1);
        }
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
#[crate::extension_point(select, adapter = crate::ContextBudgetPolicyPlugin)]
#[async_trait]
pub trait ContextBudgetPolicy: Send + Sync {
    /// Source-aware policies may lend unused quotas without duplicating assembly.
    async fn allocate_sources(
        &self,
        blocks: &[ContextBlockConfig],
        available: ContextBudget,
        _source_tokens: &[usize],
    ) -> Result<Vec<ContextBudget>, AgentError> {
        self.allocate(blocks, available).await
    }
    async fn allocate(
        &self,
        blocks: &[ContextBlockConfig],
        available: ContextBudget,
    ) -> Result<Vec<ContextBudget>, AgentError>;
}
pub struct IndependentBlockBudget;
#[async_trait]
impl ContextBudgetPolicy for IndependentBlockBudget {
    async fn allocate(
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
    pub models: &'a dyn ChatExecutor,
    pub limits: &'a RunLimits,
    pub overflow_recoveries: usize,
    pub compression: Option<ContextCompressionDirective>,
}
#[crate::extension_point(select, adapter = crate::ContextReducerPlugin)]
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
        )
        .await?;
        Ok(input.block.messages.clone())
    }
}
pub struct RecentConversation;
#[async_trait]
impl ContextReducer for RecentConversation {
    async fn reduce(&self, input: &ContextReductionInput<'_>) -> Result<Vec<Message>, AgentError> {
        let mut result = input.block.messages.clone();
        trim_context_with(&mut result, input.budget, input.model, input.estimator).await?;
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
            let size = input.estimator.message(input.model, message).await?;
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
    NoGain,
    Omitted { reason: String },
    Failed { reason: String },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSourceReport {
    pub id: String,
    pub source_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBlockReport {
    #[serde(default)]
    pub kind: ContextSourceKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<ContextSourceReport>,
    pub id: String,
    pub position: usize,
    pub source_version: Option<String>,
    pub original_tokens: Option<usize>,
    /// Resolved plugin query ceiling for this request, before shared-pool allocation.
    /// Absent in reports persisted before this field was introduced.
    #[serde(default)]
    pub configured_tokens: Option<usize>,
    pub allocated_tokens: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduction_budget_tokens: Option<usize>,
    pub final_tokens: usize,
    #[serde(default)]
    pub final_message_count: usize,
    pub outcome: ContextBlockOutcome,
    #[serde(default)]
    pub compression_trigger: ContextCompressionTrigger,
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
#[derive(Serialize, Deserialize)]
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
    pub models: &'a dyn ChatExecutor,
    pub limits: &'a RunLimits,
    pub overflow_recoveries: usize,
}
#[crate::extension_point(select, adapter = crate::ContextAssemblerPlugin)]
#[async_trait]
pub trait ContextAssembler: Send + Sync {
    async fn assemble(&self, input: &ContextAssemblyInput<'_>)
    -> Result<ContextWindow, AgentError>;
}
/// Protected source adapters use the same read contract as retrieval sources.
struct BuiltinHistory;
#[async_trait]
impl ContextContributor for BuiltinHistory {
    fn kind(&self) -> ContextSourceKind {
        ContextSourceKind::History
    }
    fn block_id(&self) -> &str {
        HISTORY_BLOCK
    }
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        input.run.check_cancelled()?;
        Ok(ContextBlock {
            id: HISTORY_BLOCK.into(),
            messages: input.history.messages.clone(),
            trust: ContextTrust::Data,
            source_version: Some(input.history.revision.to_string()),
        })
    }
}
struct BuiltinInstructions<'a> {
    seed: &'a [Message],
    sources: &'a [Arc<dyn ContextContributor>],
    provenance: std::sync::Mutex<Vec<ContextSourceReport>>,
    max_messages: usize,
    max_bytes: usize,
}
impl BuiltinInstructions<'_> {
    async fn read(
        &self,
        input: &ContextInput<'_>,
        max_messages: usize,
        max_bytes: usize,
    ) -> Result<(ContextBlock, Vec<ContextSourceReport>), AgentError> {
        let mut block = ContextBlock::instructions(INSTRUCTIONS_BLOCK, self.seed.to_vec());
        validate_contribution(&block, max_messages, max_bytes)?;
        let mut provenance = vec![ContextSourceReport {
            id: INSTRUCTIONS_BLOCK.into(),
            source_version: None,
        }];
        for source in self.sources {
            input.run.check_cancelled()?;
            let used = context_tokens(&block.messages, input.model, input.estimator).await?;
            let request =
                ContextInput {
                    budget: ContextBudget {
                        max_tokens: input.budget.max_tokens.checked_sub(used).ok_or_else(|| {
                            context_budget_error("instructions exceed source quota")
                        })?,
                        max_messages: input
                            .budget
                            .max_messages
                            .saturating_sub(block.messages.len()),
                    },
                    ..*input
                };
            let addition = source.contribute(&request).await?;
            if addition.id != source.block_id()
                || !matches!(addition.trust, ContextTrust::Instruction | ContextTrust::Mixed) {
                return Err(AgentError::InvalidConfiguration(
                    "instruction source changed its identity or trust".into(),
                ));
            }
            validate_contribution(&addition, max_messages, max_bytes)?;
            provenance.push(ContextSourceReport {
                id: addition.id,
                source_version: addition.source_version,
            });
            if addition.trust == ContextTrust::Mixed {
                block.trust = ContextTrust::Mixed;
            }
            block.messages.extend(
                addition
                    .messages
                    .into_iter()
                    .map(|message| {
                        if message.role == Role::System {
                            message.with_instruction_origin(InstructionOrigin::Plugin)
                        } else {
                            message
                        }
                    }),
            );
            if block.messages.len() > input.budget.max_messages
                || context_tokens(&block.messages, input.model, input.estimator).await?
                    > input.budget.max_tokens
            {
                return Err(context_budget_error(
                    "instruction source exceeded its quota",
                ));
            }
            validate_contribution(&block, max_messages, max_bytes)?;
        }
        Ok((block, provenance))
    }
}
#[async_trait]
impl ContextContributor for BuiltinInstructions<'_> {
    fn kind(&self) -> ContextSourceKind {
        ContextSourceKind::Instruction
    }
    fn block_id(&self) -> &str {
        INSTRUCTIONS_BLOCK
    }
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        let (block, provenance) = self.read(input, self.max_messages, self.max_bytes).await?;
        *self.provenance.lock().expect("instruction provenance lock") = provenance;
        Ok(block)
    }
}
pub struct DefaultContextAssembler {
    contributors: HashMap<String, Arc<dyn ContextContributor>>,
    instruction_sources: Vec<Arc<dyn ContextContributor>>,
    layout: ContextLayout,
    policy: Arc<dyn ContextBudgetPolicy>,
    history_reducer: Arc<dyn ContextReducer>,
    pub global_compression_ratio_bps: Option<u16>,
    pub force_global_compression: bool,
    pub source_timeout: Duration,
    pub max_source_messages: usize,
    pub max_source_bytes: usize,
}
impl Default for DefaultContextAssembler {
    fn default() -> Self {
        Self {
            contributors: HashMap::new(),
            instruction_sources: Vec::new(),
            layout: ContextLayout::default(),
            policy: Arc::new(IndependentBlockBudget),
            history_reducer: Arc::new(RecentConversation),
            global_compression_ratio_bps: None,
            force_global_compression: false,
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
            if contributor.kind() == ContextSourceKind::History {
                return Err(AgentError::InvalidConfiguration(
                    "history sources are owned by the SDK".into(),
                ));
            }
            if contributor.kind() == ContextSourceKind::Instruction {
                if id.trim().is_empty()
                    || is_builtin(&id)
                    || result
                        .instruction_sources
                        .iter()
                        .any(|source| source.block_id() == id)
                {
                    return Err(AgentError::InvalidConfiguration(
                        "invalid or duplicate instruction source ID".into(),
                    ));
                }
                result.instruction_sources.push(contributor);
                continue;
            }
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
                    || (block.id == INSTRUCTIONS_BLOCK && block.compression.is_some())
                    || block.compression.is_some_and(|policy| {
                        policy.trigger_ratio_bps > 10_000
                            || policy.target_ratio_bps >= policy.trigger_ratio_bps
                    })
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
    pub fn with_global_compression(
        mut self,
        ratio_bps: Option<u16>,
        force: bool,
    ) -> Result<Self, AgentError> {
        if ratio_bps.is_some_and(|ratio| ratio > 10_000) {
            return Err(AgentError::InvalidConfiguration(
                "invalid global compression percentage".into(),
            ));
        }
        self.global_compression_ratio_bps = ratio_bps;
        self.force_global_compression = force;
        Ok(self)
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

impl DefaultContextAssembler {
    async fn assemble_inner(
        &self,
        input: &ContextAssemblyInput<'_>,
    ) -> Result<ContextWindow, AgentError> {
        input.run.check_cancelled()?;
        validate_messages(&input.history.messages).map_err(ContextError::from)?;
        let protocol = input
            .estimator
            .tools(input.model, input.tools)
            .await?
            .saturating_add(input.estimator.options(input.model, input.options).await?);
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
        let implicit_layout = self.layout.0.is_empty();
        let mut layout = if implicit_layout {
            vec![
                ContextBlockConfig::new(
                    INSTRUCTIONS_BLOCK,
                    BlockBudget::tokens(available.max_tokens),
                ),
                ContextBlockConfig {
                    id: HISTORY_BLOCK.into(),
                    budget: BlockBudget::tokens(available.max_tokens),
                    failure: BlockFailure::Error,
                    reducer: self.history_reducer.clone(),
                    compression: None,
                },
            ]
        } else {
            self.layout.0.clone()
        };
        let scoped = input.run.extensions.get::<ScopedAgentServices>();
        let mut instruction_sources = self.instruction_sources.clone();
        if let Some(services) = &scoped {
            instruction_sources.extend(
                services
                    .0
                    .context_sources
                    .iter()
                    .filter(|source| source.kind() == ContextSourceKind::Instruction)
                    .cloned(),
            );
        }
        let mut instruction_ids = HashSet::new();
        for source in &instruction_sources {
            if source.block_id().trim().is_empty()
                || is_builtin(source.block_id())
                || !instruction_ids.insert(source.block_id())
            {
                return Err(AgentError::InvalidConfiguration(
                    "invalid or duplicate instruction source ID".into(),
                ));
            }
        }
        let empty = Content::default();
        let current = input
            .history
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map_or(&empty, |message| &message.content);
        let mut sources = Vec::new();
        let mut reports = Vec::new();
        // Phase one: each provider is read exactly once, under its own query cap.
        for (position, config) in layout.iter().enumerate() {
            let query_budget = configured_block_budget(config, available)?;
            let mut report = ContextBlockReport {
                id: config.id.clone(),
                kind: if config.id == INSTRUCTIONS_BLOCK {
                    ContextSourceKind::Instruction
                } else if config.id == HISTORY_BLOCK {
                    ContextSourceKind::History
                } else {
                    ContextSourceKind::Data
                },
                sources: Vec::new(),
                position,
                source_version: None,
                original_tokens: None,
                configured_tokens: Some(query_budget.max_tokens),
                allocated_tokens: query_budget.max_tokens,
                reduction_budget_tokens: None,
                final_tokens: 0,
                final_message_count: 0,
                outcome: ContextBlockOutcome::Kept,
                compression_trigger: ContextCompressionTrigger::None,
            };
            if config.id == INSTRUCTIONS_BLOCK {
                report.sources = std::iter::once(ContextSourceReport {
                    id: INSTRUCTIONS_BLOCK.into(),
                    source_version: None,
                })
                .chain(
                    instruction_sources
                        .iter()
                        .map(|source| ContextSourceReport {
                            id: source.block_id().into(),
                            source_version: None,
                        }),
                )
                .collect();
            }
            let load = async {
                let request = ContextInput {
                    stage: input.stage,
                    step: input.step,
                    model: input.model,
                    budget: query_budget,
                    current_input: current,
                    history: input.history,
                    estimator: input.estimator,
                    run: input.run,
                };
                let instructions = BuiltinInstructions {
                    seed: input.instructions,
                    sources: &instruction_sources,
                    provenance: std::sync::Mutex::new(Vec::new()),
                    max_messages: self.max_source_messages,
                    max_bytes: self.max_source_bytes,
                };
                let history = BuiltinHistory;
                let source: &dyn ContextContributor = if config.id == INSTRUCTIONS_BLOCK {
                    &instructions
                } else if config.id == HISTORY_BLOCK {
                    &history
                } else {
                    self.contributors[&config.id].as_ref()
                };
                let block = source.contribute(&request).await?;
                match report.kind {
                    ContextSourceKind::Instruction => {
                        if !matches!(block.trust, ContextTrust::Instruction | ContextTrust::Mixed) {
                            return Err(AgentError::InvalidConfiguration(
                                "instruction source changed its trust".into(),
                            ));
                        }
                        validate_contribution(
                            &block,
                            self.max_source_messages,
                            self.max_source_bytes,
                        )?;
                        report.sources = instructions
                            .provenance
                            .into_inner()
                            .expect("instruction provenance lock");
                    }
                    ContextSourceKind::History => {
                        validate_messages(&block.messages).map_err(ContextError::from)?;
                    }
                    ContextSourceKind::Data => {
                        if block.trust != ContextTrust::Data {
                            return Err(AgentError::InvalidConfiguration(
                                "data source changed its trust".into(),
                            ));
                        }
                        validate_contribution(
                            &block,
                            self.max_source_messages,
                            self.max_source_bytes,
                        )?;
                    }
                }
                if config.id != INSTRUCTIONS_BLOCK {
                    report.sources.push(ContextSourceReport {
                        id: block.id.clone(),
                        source_version: block.source_version.clone(),
                    });
                }
                if block.id != config.id {
                    return Err(AgentError::InvalidConfiguration(
                        "contributor changed its block ID".into(),
                    ));
                }
                if block.trust != ContextTrust::Data && config.compression.is_some() {
                    return Err(AgentError::InvalidConfiguration(
                        "instruction providers cannot be compressed".into(),
                    ));
                }
                report.source_version = block.source_version.clone();
                report.original_tokens =
                    Some(context_tokens(&block.messages, input.model, input.estimator).await?);
                Ok(block)
            };
            let deadline =
                (tokio::time::Instant::now() + self.source_timeout).min(input.run.deadline);
            let loaded = tokio::select! {
                biased;
                _ = input.run.cancelled() => Err(AgentError::Cancelled),
                result = tokio::time::timeout_at(deadline, load) => result.unwrap_or_else(|_| Err(AgentError::Run("context block processing timed out".into()))),
            };
            match loaded {
                Ok(block) => sources.push(Some(block)),
                Err(error) if can_omit_block(config, &error) => {
                    report.outcome = ContextBlockOutcome::Omitted {
                        reason: error.kind().into(),
                    };
                    sources.push(None);
                }
                Err(error) => {
                    report.outcome = ContextBlockOutcome::Failed {
                        reason: error.kind().into(),
                    };
                    reports.push(report);
                    return Err(context_assembly_failure(input, reports, error));
                }
            }
            reports.push(report);
        }
        if implicit_layout {
            let fixed = reports[0].original_tokens.unwrap_or(0);
            layout[0].budget = BlockBudget {
                max_messages: Some(sources[0].as_ref().map_or(0, |b| b.messages.len())),
                ..BlockBudget::tokens(fixed)
            };
            layout[1].budget = BlockBudget {
                max_messages: Some(
                    available
                        .max_messages
                        .saturating_sub(sources[0].as_ref().map_or(0, |b| b.messages.len())),
                ),
                ..BlockBudget::tokens(available.max_tokens.checked_sub(fixed).ok_or_else(|| {
                    context_assembly_failure(
                        input,
                        reports.clone(),
                        context_budget_error("instructions exceed input capacity"),
                    )
                })?)
            };
        }
        let mut source_tokens: Vec<_> = reports
            .iter()
            .map(|report| report.original_tokens.unwrap_or(0))
            .collect();
        let original_total = source_tokens
            .iter()
            .fold(protocol, |total, tokens| total.saturating_add(*tokens));
        let global = self.force_global_compression
            || self.global_compression_ratio_bps.is_some_and(|ratio| {
                original_total
                    >= ((input.budget.max_tokens as u128 * ratio as u128) / 10_000) as usize
            });
        let mut allocations = self
            .policy
            .allocate_sources(&layout, available, &source_tokens)
            .await?;
        validate_allocations(&allocations, layout.len(), available)?;
        let mut outputs = vec![Vec::new(); layout.len()];
        // Reduction order is independent of prompt order. Reducing fixed data first
        // lets history borrow newly freed capacity without reading sources again.
        let reduction_order: Vec<_> = (0..layout.len())
            .filter(|index| layout[*index].id != HISTORY_BLOCK)
            .chain((0..layout.len()).filter(|index| layout[*index].id == HISTORY_BLOCK))
            .collect();
        for position in reduction_order {
            let config = &layout[position];
            let Some(block) = &sources[position] else {
                continue;
            };
            if config.id == HISTORY_BLOCK {
                allocations = self
                    .policy
                    .allocate_sources(&layout, available, &source_tokens)
                    .await?;
                validate_allocations(&allocations, layout.len(), available)?;
            }
            let budget = allocations[position];
            // History borrows actual remaining capacity; its pressure is relative
            // to that quota. Fixed provider thresholds retain their configured cap.
            let nominal = if config.id == HISTORY_BLOCK {
                budget
            } else {
                configured_block_budget(config, available)?
            };
            let trigger = config.compression.and_then(|policy| {
                let reached = source_tokens[position]
                    >= ((nominal.max_tokens as u128 * policy.trigger_ratio_bps as u128) / 10_000)
                        as usize;
                (global || reached).then_some(ContextCompressionDirective {
                    trigger: if global {
                        ContextCompressionTrigger::Global
                    } else {
                        ContextCompressionTrigger::Block
                    },
                    target_tokens: ((nominal.max_tokens as u128 * policy.target_ratio_bps as u128)
                        / 10_000) as usize,
                })
            });
            reports[position].allocated_tokens = budget.max_tokens;
            reports[position].reduction_budget_tokens = Some(budget.max_tokens);
            reports[position].compression_trigger = trigger
                .map_or(ContextCompressionTrigger::None, |directive| {
                    directive.trigger
                });
            let process = async {
                let reduction = ContextReductionInput {
                    block,
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
                    compression: trigger,
                };
                let mut output = if block.trust != ContextTrust::Data {
                    KeepContext.reduce(&reduction).await?
                } else {
                    config.reducer.reduce(&reduction).await?
                };
                validate_messages(&output).map_err(ContextError::from)?;
                if block.trust != ContextTrust::Data && output != block.messages {
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
                if trigger.is_some()
                    && reports[position]
                        .original_tokens
                        .unwrap_or(0)
                        .saturating_sub(
                            context_tokens(&output, input.model, input.estimator).await?,
                        )
                        < config
                            .compression
                            .map_or(1, |policy| policy.min_gain_tokens)
                {
                    output = block.messages.clone();
                }
                ensure_context_fits(&output, budget, input.model, input.estimator).await?;
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
                Ok(output) => {
                    let tokens = context_tokens(&output, input.model, input.estimator).await?;
                    reports[position].final_tokens = tokens;
                    reports[position].final_message_count = output.len();
                    reports[position].outcome = if output != block.messages {
                        ContextBlockOutcome::Reduced
                    } else if trigger.is_some() {
                        ContextBlockOutcome::NoGain
                    } else {
                        ContextBlockOutcome::Kept
                    };
                    source_tokens[position] = tokens;
                    outputs[position] = output;
                }
                Err(error) if can_omit_block(config, &error) => {
                    reports[position].outcome = ContextBlockOutcome::Omitted {
                        reason: error.kind().into(),
                    };
                    source_tokens[position] = 0;
                }
                Err(error) => {
                    reports[position].outcome = ContextBlockOutcome::Failed {
                        reason: error.kind().into(),
                    };
                    return Err(context_assembly_failure(input, reports, error));
                }
            }
        }
        let final_allocations = self
            .policy
            .allocate_sources(&layout, available, &source_tokens)
            .await?;
        validate_allocations(&final_allocations, layout.len(), available)?;
        for (position, budget) in final_allocations.into_iter().enumerate() {
            reports[position].allocated_tokens = budget.max_tokens;
            if let Err(error) =
                ensure_context_fits(&outputs[position], budget, input.model, input.estimator).await
            {
                reports[position].outcome = ContextBlockOutcome::Failed {
                    reason: error.kind().into(),
                };
                return Err(context_assembly_failure(input, reports, error));
            }
        }
        let messages: Vec<_> = outputs.into_iter().flatten().collect();
        ensure_context_fits(&messages, available, input.model, input.estimator).await?;
        validate_messages(&messages).map_err(ContextError::from)?;
        let total = context_tokens(&messages, input.model, input.estimator)
            .await?
            .saturating_add(protocol);
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
#[async_trait]
impl ContextAssembler for DefaultContextAssembler {
    async fn assemble(
        &self,
        input: &ContextAssemblyInput<'_>,
    ) -> Result<ContextWindow, AgentError> {
        // This outer guard also covers custom budget policies and estimators,
        // which are not part of an individual provider/reducer timeout.
        tokio::select! {
            biased;
            _ = input.run.cancelled() => Err(AgentError::Cancelled),
            result = tokio::time::timeout_at(input.run.deadline, self.assemble_inner(input)) => result.unwrap_or_else(|_| Err(AgentError::Run("context assembly timed out".into()))),
        }
    }
}

/// The query cap and pressure threshold are independent of unused-quota lending.
pub fn configured_block_budget(
    config: &ContextBlockConfig,
    available: ContextBudget,
) -> Result<ContextBudget, AgentError> {
    if config
        .budget
        .max_ratio_bps
        .is_some_and(|ratio| ratio > 10_000)
        || (config.budget.max_tokens.is_none() && config.budget.max_ratio_bps.is_none())
    {
        return Err(AgentError::InvalidConfiguration(
            "invalid context block quota".into(),
        ));
    }
    let ratio = config
        .budget
        .max_ratio_bps
        .map(|ratio| ((available.max_tokens as u128 * ratio as u128) / 10_000) as usize);
    Ok(ContextBudget {
        max_tokens: config
            .budget
            .max_tokens
            .unwrap_or(usize::MAX)
            .min(ratio.unwrap_or(usize::MAX))
            .min(available.max_tokens),
        max_messages: config
            .budget
            .max_messages
            .unwrap_or(available.max_messages)
            .min(available.max_messages),
    })
}
fn validate_allocations(
    allocations: &[ContextBudget],
    count: usize,
    available: ContextBudget,
) -> Result<(), AgentError> {
    if allocations.len() != count
        || allocations
            .iter()
            .try_fold(0usize, |total, budget| total.checked_add(budget.max_tokens))
            .is_none_or(|total| total > available.max_tokens)
    {
        Err(context_budget_error(
            "budget policy returned invalid quotas",
        ))
    } else {
        Ok(())
    }
}
fn can_omit_block(config: &ContextBlockConfig, error: &AgentError) -> bool {
    config.failure == BlockFailure::Omit
        && !matches!(
            error.primary(),
            AgentError::Cancelled
                | AgentError::LongTermMemory(crate::MemoryError::Conflict)
                | AgentError::Context(ContextError::RefreshRequired { .. })
        )
        && !error.contains_state_error()
}
fn context_assembly_failure(
    input: &ContextAssemblyInput<'_>,
    reports: Vec<ContextBlockReport>,
    error: AgentError,
) -> AgentError {
    AgentError::ContextAssembly {
        report: Box::new(ContextFailureReport::new(
            input.model.clone(),
            input.stage.into(),
            input.step,
            input.attempt,
            input.run.node_execution().cloned(),
            reports,
            &error,
        )),
        source: Box::new(error),
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
    if block.trust == ContextTrust::Mixed
        && (block.messages.first().is_none_or(|message| message.role != Role::System)
            || !block.messages.iter().any(|message| message.role == Role::User))
    {
        return Err(AgentError::InvalidConfiguration(
            "mixed context must start with an instruction and contain lower-trust data".into(),
        ));
    }
    if block.messages.len() > max_messages
        || serde_json::to_writer(ByteLimit(max_bytes), &block.messages).is_err()
    {
        return Err(context_budget_error(
            "source exceeds configured size limits",
        ));
    }
    if block.messages.iter().any(|m| {
        (match block.trust {
            ContextTrust::Instruction => m.role != Role::System,
            ContextTrust::Data => m.role != Role::User,
            ContextTrust::Mixed => !matches!(m.role, Role::System | Role::User),
        }) || !m.tool_calls.is_empty()
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
        .filter(|m| m.role == Role::System)
        .collect();
    let projected: Vec<_> = output
        .iter()
        .filter(|m| m.role == Role::System)
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
pub async fn context_tokens(
    messages: &[Message],
    model: &ModelRef,
    estimator: &dyn TokenEstimator,
) -> Result<usize, AgentError> {
    let mut total = 0usize;
    for message in messages {
        total = total.saturating_add(estimator.message(model, message).await?);
    }
    Ok(total)
}
pub async fn ensure_context_fits(
    messages: &[Message],
    budget: ContextBudget,
    model: &ModelRef,
    estimator: &dyn TokenEstimator,
) -> Result<(), AgentError> {
    if messages.len() > budget.max_messages
        || context_tokens(messages, model, estimator).await? > budget.max_tokens
    {
        return Err(context_budget_error("context block exceeds its quota"));
    }
    Ok(())
}
fn context_budget_error(message: &str) -> AgentError {
    ContextError::BudgetExceeded(message.into()).into()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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

pub async fn trim_context_with(
    messages: &mut Vec<Message>,
    budget: ContextBudget,
    model: &ModelRef,
    estimator: &dyn TokenEstimator,
) -> Result<(), AgentError> {
    while messages.len() > budget.max_messages
        || context_tokens(messages, model, estimator).await? > budget.max_tokens
    {
        if !remove_oldest_turn(messages) {
            return Err(ContextError::BudgetExceeded(
                "fixed instructions and newest turn exceed the model-specific context budget"
                    .into(),
            )
            .into());
        }
    }
    Ok(())
}

#[doc(hidden)]
pub fn remove_oldest_turn(messages: &mut Vec<Message>) -> bool {
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
        let retain = !range.contains(&index) || message.role == Role::System;
        index += 1;
        retain
    });
    true
}
