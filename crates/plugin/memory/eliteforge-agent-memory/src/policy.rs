//! Product-independent block budgets and request accounting.
use serde::Serialize;
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct ContextBlockBudgetConfig {
    pub max_ratio_bps: Option<u32>,
    pub max_tokens: Option<u32>,
}
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct ContextBlockBudgets {
    pub seed_prompt_prefix: ContextBlockBudgetConfig,
    pub current_session_checkpoint: ContextBlockBudgetConfig,
    pub session_family_context: ContextBlockBudgetConfig,
    pub current_session_working_state: ContextBlockBudgetConfig,
    pub tool_schema: ContextBlockBudgetConfig,
}
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct ModelContextBudgetPlan {
    pub model_context_window: u32,
    pub output_token_reserve: u32,
    pub effective_input_budget: u32,
    pub block_budgets: ContextBlockBudgetPlans,
}
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct ContextBlockBudgetPlans {
    pub seed_prompt_prefix: ContextBlockBudgetPlan,
    pub current_session_checkpoint: ContextBlockBudgetPlan,
    pub session_family_context: ContextBlockBudgetPlan,
    pub current_session_working_state: ContextBlockBudgetPlan,
    pub tool_schema: ContextBlockBudgetPlan,
    pub current_session_history: ContextBlockBudgetPlan,
}
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct ContextBlockBudgetPlan {
    pub block_kind: &'static str,
    pub enabled: bool,
    pub effective_limit_tokens: u32,
    pub priority: u8,
    pub allow_borrow_unused_budget: bool,
}
impl Default for ContextBlockBudgets {
    fn default() -> Self {
        let cap = |ratio, tokens| ContextBlockBudgetConfig {
            max_ratio_bps: Some(ratio),
            max_tokens: tokens,
        };
        Self {
            seed_prompt_prefix: cap(5_000, Some(16_000)),
            current_session_checkpoint: cap(2_000, Some(6_000)),
            session_family_context: cap(1_000, Some(12_000)),
            current_session_working_state: cap(1_000, Some(2_000)),
            tool_schema: cap(10_000, None),
        }
    }
}
impl ContextBlockBudgets {
    pub fn plan(&self, context_window: u32, output_reserve: u32) -> ModelContextBudgetPlan {
        let available = context_window.saturating_sub(output_reserve);
        let block = |kind, config: &ContextBlockBudgetConfig| {
            let ratio = config.max_ratio_bps.map(|r| ratio_tokens(available, r));
            let tokens = match (ratio, config.max_tokens) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) | (None, Some(a)) => a,
                _ => available,
            }
            .min(available);
            ContextBlockBudgetPlan {
                block_kind: kind,
                enabled: true,
                effective_limit_tokens: tokens,
                priority: 0,
                allow_borrow_unused_budget: false,
            }
        };
        ModelContextBudgetPlan {
            model_context_window: context_window,
            output_token_reserve: output_reserve,
            effective_input_budget: available,
            block_budgets: ContextBlockBudgetPlans {
                seed_prompt_prefix: block("seed_prompt_prefix", &self.seed_prompt_prefix),
                current_session_checkpoint: block(
                    "current_session_checkpoint",
                    &self.current_session_checkpoint,
                ),
                session_family_context: block(
                    "session_family_context",
                    &self.session_family_context,
                ),
                current_session_working_state: block(
                    "current_session_working_state",
                    &self.current_session_working_state,
                ),
                tool_schema: block("tool_schema", &self.tool_schema),
                current_session_history: ContextBlockBudgetPlan {
                    block_kind: "current_session_history",
                    enabled: true,
                    effective_limit_tokens: available,
                    priority: 60,
                    allow_borrow_unused_budget: true,
                },
            },
        }
    }
}
pub fn ratio_tokens(tokens: u32, ratio_bps: u32) -> u32 {
    ((u64::from(tokens) * u64::from(ratio_bps.min(10_000))) / 10_000) as u32
}
/// Conservative byte-based estimate includes serialized tool/attachment metadata.
pub fn estimate_text_tokens(text: &str) -> u32 {
    ((text.len().saturating_add(3) / 4).min(u32::MAX as usize) as u32).max(1)
}
/// A hard character bound: even the marker fits in the available quota.
pub fn bound_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.into();
    }
    const SUFFIX: &str = "\n...[content truncated]";
    if max_chars <= SUFFIX.chars().count() {
        return text.chars().take(max_chars).collect();
    }
    let mut result: String = text
        .chars()
        .take(max_chars - SUFFIX.chars().count())
        .collect();
    result.push_str(SUFFIX);
    result
}
#[derive(Debug, Clone, Copy)]
pub struct RequestBudgetAssessment {
    pub estimated_tokens: u32,
    pub history_budget_tokens: u32,
    pub trigger_tokens: u32,
    pub retained_history_tokens: u32,
    pub trigger_reached: bool,
    pub over_budget: bool,
    pub reason: Option<&'static str>,
}
// Keep the public scalar adapter stable for hosts mapping separate block counters and policy ratios.
#[allow(clippy::too_many_arguments)]
pub fn assess_request(
    plan: &ModelContextBudgetPlan,
    message_tokens: u32,
    prefix_tokens: u32,
    fixed_tokens: u32,
    tool_tokens: u32,
    trigger_bps: u32,
    retained_bps: u32,
    has_compressible_history: bool,
) -> RequestBudgetAssessment {
    let estimated_tokens = message_tokens.saturating_add(tool_tokens);
    let history_budget_tokens = plan
        .effective_input_budget
        .saturating_sub(fixed_tokens.saturating_add(tool_tokens));
    let trigger_tokens = ratio_tokens(plan.effective_input_budget, trigger_bps);
    let reason = if tool_tokens > plan.block_budgets.tool_schema.effective_limit_tokens {
        Some("TOOL_SCHEMA_OVER_BUDGET")
    } else if prefix_tokens > plan.block_budgets.seed_prompt_prefix.effective_limit_tokens {
        Some("SEED_PROMPT_PREFIX_OVER_BUDGET")
    } else if estimated_tokens > plan.effective_input_budget {
        Some(if has_compressible_history {
            "COMPRESSED_HISTORY_OVER_EFFECTIVE_BUDGET"
        } else {
            "NON_COMPRESSIBLE_CONTEXT_OVER_EFFECTIVE_BUDGET"
        })
    } else {
        None
    };
    RequestBudgetAssessment {
        estimated_tokens,
        history_budget_tokens,
        trigger_tokens,
        retained_history_tokens: ratio_tokens(history_budget_tokens, retained_bps),
        trigger_reached: estimated_tokens >= trigger_tokens,
        over_budget: reason.is_some(),
        reason,
    }
}

/// All fixed blocks consume the history remainder before retention is applied.
pub fn remaining_history_budget(effective_input_budget: u32, fixed_block_tokens: &[u32]) -> u32 {
    effective_input_budget.saturating_sub(
        fixed_block_tokens
            .iter()
            .fold(0u32, |sum, t| sum.saturating_add(*t)),
    )
}
pub fn budget_reason(
    input_exceeded: bool,
    prefix_exceeded: bool,
    tools_exceeded: bool,
    has_compressible_history: bool,
) -> Option<&'static str> {
    if tools_exceeded {
        Some("TOOL_SCHEMA_OVER_BUDGET")
    } else if prefix_exceeded {
        Some("SEED_PROMPT_PREFIX_OVER_BUDGET")
    } else if input_exceeded {
        Some(if has_compressible_history {
            "COMPRESSED_HISTORY_OVER_EFFECTIVE_BUDGET"
        } else {
            "NON_COMPRESSIBLE_CONTEXT_OVER_EFFECTIVE_BUDGET"
        })
    } else {
        None
    }
}

/// Budget distribution across host-authorized related-memory contributors.
pub fn weighted_token_budget(total: u32, weight: f64, total_weight: f64) -> u32 {
    if !weight.is_finite() || weight <= 0.0 || !total_weight.is_finite() || total_weight <= 0.0 {
        return 0;
    }
    ((f64::from(total) * (weight / total_weight).min(1.0)).round() as u32).min(total)
}
pub fn append_bounded_text(
    rendered: &mut String,
    used: &mut usize,
    max_bytes: usize,
    text: &str,
) -> bool {
    if used.saturating_add(text.len()) > max_bytes {
        return false;
    }
    rendered.push_str(text);
    *used = used.saturating_add(text.len());
    true
}
/// Preserve framing and enforce UTF-8 byte caps for nested source sections.
#[allow(clippy::too_many_arguments)]
pub fn append_bounded_section(
    rendered: &mut String,
    used: &mut usize,
    local_used: &mut usize,
    max_bytes: usize,
    local_max_bytes: usize,
    open: &str,
    content: &str,
    close: &str,
    content_max_chars: usize,
) -> bool {
    let available = max_bytes
        .saturating_sub(*used)
        .min(local_max_bytes.saturating_sub(*local_used));
    let overhead = open.len().saturating_add(close.len());
    if available <= overhead {
        return false;
    }
    let mut content = bound_text(content, content_max_chars.min(available - overhead));
    if content.len() > available - overhead {
        let mut end = (available - overhead).min(content.len());
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
    }
    let block = format!("{open}{content}{close}");
    rendered.push_str(&block);
    *used = used.saturating_add(block.len());
    *local_used = local_used.saturating_add(block.len());
    true
}

/// Legacy checkpoint summaries may include retained text; share the quota only
/// when both fields contain data. New head-only checkpoints use the full cap.
pub fn bound_checkpoint_data(summary: &str, recent: &str, max_chars: usize) -> (String, String) {
    let summary_limit = if recent.is_empty() {
        max_chars
    } else {
        max_chars / 2
    };
    let recent_limit = if summary.is_empty() {
        max_chars
    } else {
        max_chars - summary_limit
    };
    (
        bound_text(summary, summary_limit),
        bound_text(recent, recent_limit),
    )
}
