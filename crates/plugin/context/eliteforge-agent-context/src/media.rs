//! Data-only multimodal admission using the model's native token estimator.
//! Hosts resolve authorized media; this module never fetches files or URLs.
use async_trait::async_trait;
use eliteforge_agent_core::*;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaAdmissionOutcome {
    Included,
    Budget,
    Quota,
    UnknownCost,
    UnsupportedModel,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaAdmission {
    pub name: Option<String>,
    pub outcome: MediaAdmissionOutcome,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaContextReport {
    pub block_id: String,
    pub allocated_tokens: usize,
    pub final_tokens: usize,
    pub media: Vec<MediaAdmission>,
}
/// Optional per-run audit handle. Reports contain identities and costs, never media bytes.
#[derive(Default, Clone)]
pub struct MediaContextAudit(pub Arc<Mutex<Vec<MediaContextReport>>>);
pub struct MultimodalDataProjection {
    pub messages: Vec<Message>,
    pub report: MediaContextReport,
}
/// Text gets a bounded reservation; each media block is then admitted atomically.
/// Text captions survive omission and the model receives an explicit omission notice.
pub struct BoundMultimodalData {
    pub max_media: usize,
    pub text_tokens: usize,
}
impl Default for BoundMultimodalData {
    fn default() -> Self {
        Self {
            max_media: 4,
            text_tokens: 1200,
        }
    }
}
#[async_trait]
impl ContextReducer for BoundMultimodalData {
    async fn reduce(&self, input: &ContextReductionInput<'_>) -> Result<Vec<Message>, AgentError> {
        input.run.check_cancelled()?;
        let budget = ContextBudget {
            max_tokens: input.compression.map_or(input.budget.max_tokens, |d| {
                d.target_tokens.min(input.budget.max_tokens)
            }),
            ..input.budget
        };
        let accepts_media = input
            .models
            .catalog()
            .get(input.model)
            .is_some_and(|c| c.input_modalities.contains(&Modality::Image));
        let projected = tokio::select! {
            biased;
            _=input.run.cancelled()=>return Err(AgentError::Cancelled),
            value=project_multimodal_data(input.block,budget,input.model,input.estimator,self.max_media,self.text_tokens,accepts_media)=>value?,
        };
        if let Some(audit) = input.run.extensions.get::<MediaContextAudit>() {
            audit
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(projected.report);
        }
        input.run.check_cancelled()?;
        Ok(projected.messages)
    }
}
/// Native projection is also usable by pure callers without a model invocation.
pub async fn project_multimodal_data(
    block: &ContextBlock,
    budget: ContextBudget,
    model: &ModelRef,
    estimator: &dyn TokenEstimator,
    max_media: usize,
    text_tokens: usize,
    accepts_media: bool,
) -> Result<MultimodalDataProjection, AgentError> {
    if block.trust != ContextTrust::Data
        || block.messages.iter().any(|m| {
            m.role != Role::User
                || !m.tool_calls.is_empty()
                || m.tool_call_id.is_some()
                || m.structured.is_some()
                || m.tool_result.is_some()
                || m.model_context.is_some()
        })
    {
        return Err(AgentError::InvalidConfiguration(
            "multimodal bounding accepts data-only user messages".into(),
        ));
    }
    let mut media = Vec::new();
    let mut text = String::new();
    for message in &block.messages {
        for content in &message.content.0 {
            match content {
                ContentBlock::Text { text: value } => {
                    text.push_str(value);
                    text.push('\n');
                }
                ContentBlock::Media { media: value } => media.push(value.clone()),
                _ => {
                    return Err(AgentError::InvalidConfiguration(
                        "multimodal data requires text or resolved media".into(),
                    ));
                }
            }
        }
    }
    // Reserve the notice before selection so omissions never silently imply vision.
    let notice = "[Visual evidence admission is budgeted. Images absent from this message were not shown to the model.]";
    let base = if media.is_empty() {
        text
    } else {
        format!("{notice}\n{text}")
    };
    let chars: Vec<_> = base.chars().collect();
    let mut low = 0;
    let mut high = chars.len();
    let mut bounded = Message::user("");
    let target = budget.max_tokens.min(text_tokens.max(if media.is_empty() {
        0
    } else {
        notice.len() + 20
    }));
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let candidate = Message::user(chars[..mid].iter().collect::<String>());
        if estimator.message(model, &candidate).await? <= target {
            low = mid;
            bounded = candidate;
        } else {
            high = mid - 1;
        }
    }
    let mut cost = estimator.message(model, &bounded).await?;
    let mut admissions = Vec::new();
    let mut included = 0;
    if budget.max_messages == 0 || cost > budget.max_tokens {
        return Ok(MultimodalDataProjection {
            messages: vec![],
            report: MediaContextReport {
                block_id: block.id.clone(),
                allocated_tokens: budget.max_tokens,
                final_tokens: 0,
                media: media
                    .into_iter()
                    .map(|m| MediaAdmission {
                        name: m.name,
                        outcome: MediaAdmissionOutcome::Budget,
                    })
                    .collect(),
            },
        });
    }
    for item in media {
        let outcome = if !accepts_media {
            MediaAdmissionOutcome::UnsupportedModel
        } else if item.estimated_tokens.is_none() {
            MediaAdmissionOutcome::UnknownCost
        } else if included >= max_media {
            MediaAdmissionOutcome::Quota
        } else {
            let mut candidate = bounded.clone();
            // Wire adapters may discard MediaContent.name. Bind the citation to
            // the actual included image with adjacent native text as well.
            if let Some(name) = &item.name {
                candidate.content.0.push(ContentBlock::Text {
                    text: format!("[Included visual evidence: {name}]"),
                });
            }
            candidate.content.0.push(ContentBlock::Media {
                media: item.clone(),
            });
            let estimate = estimator.message(model, &candidate).await?;
            if estimate <= budget.max_tokens {
                bounded = candidate;
                cost = estimate;
                included += 1;
                MediaAdmissionOutcome::Included
            } else {
                MediaAdmissionOutcome::Budget
            }
        };
        admissions.push(MediaAdmission {
            name: item.name,
            outcome,
        });
    }
    // The notice is intentionally retained even when all media fits: it defines the
    // scope of evidence, without claiming a retrieved caption is a viewed image.
    let final_tokens = if bounded.content.is_empty() { 0 } else { cost };
    Ok(MultimodalDataProjection {
        messages: if bounded.content.is_empty() {
            vec![]
        } else {
            vec![bounded]
        },
        report: MediaContextReport {
            block_id: block.id.clone(),
            allocated_tokens: budget.max_tokens,
            final_tokens,
            media: admissions,
        },
    })
}

/// Borrow unused history capacity for one optional data source, while preserving
/// the complete latest user turn (including its tool protocol continuation).
/// Allocation and final media admission remain independently enforced by SDK.
pub struct BorrowHistoryForData {
    pub base: Arc<dyn ContextBudgetPolicy>,
    pub block_id: String,
    pub max_tokens: usize,
    pub protected_history_tokens: usize,
}
#[async_trait]
impl ContextBudgetPolicy for BorrowHistoryForData {
    async fn allocate(
        &self,
        blocks: &[ContextBlockConfig],
        available: ContextBudget,
    ) -> Result<Vec<ContextBudget>, AgentError> {
        self.base.allocate(blocks, available).await
    }
    async fn allocate_sources(
        &self,
        blocks: &[ContextBlockConfig],
        available: ContextBudget,
        costs: &[usize],
    ) -> Result<Vec<ContextBudget>, AgentError> {
        let mut budgets = self.base.allocate_sources(blocks, available, costs).await?;
        if blocks.len() != costs.len() || budgets.len() != blocks.len() {
            return Err(AgentError::InvalidConfiguration(
                "data borrowing requires matching source budgets".into(),
            ));
        }
        let data = blocks
            .iter()
            .position(|b| b.id == self.block_id)
            .ok_or_else(|| {
                AgentError::InvalidConfiguration("data borrowing block is absent".into())
            })?;
        let history = blocks
            .iter()
            .position(|b| b.id == HISTORY_BLOCK)
            .ok_or_else(|| {
                AgentError::InvalidConfiguration("data borrowing requires history".into())
            })?;
        if data == history || blocks[data].id == INSTRUCTIONS_BLOCK {
            return Err(AgentError::InvalidConfiguration(
                "only optional data may borrow history capacity".into(),
            ));
        }
        let ceiling = configured_block_budget(&blocks[data], available)?
            .max_tokens
            .min(self.max_tokens);
        let desired = costs[data].min(ceiling);
        let borrowed = desired.saturating_sub(budgets[data].max_tokens).min(
            budgets[history]
                .max_tokens
                .saturating_sub(self.protected_history_tokens),
        );
        budgets[data].max_tokens = budgets[data].max_tokens.saturating_add(borrowed);
        budgets[history].max_tokens -= borrowed;
        Ok(budgets)
    }
}
/// The entire latest user turn is protected, including following assistant/tool
/// messages; an opaque history without a user boundary is protected in full.
pub async fn required_recent_turn_tokens(
    messages: &[Message],
    model: &ModelRef,
    estimator: &dyn TokenEstimator,
) -> Result<usize, ModelError> {
    let start = messages
        .iter()
        .rposition(|m| m.role == Role::User)
        .unwrap_or(0);
    let mut cost = 0usize;
    for message in &messages[start..] {
        cost = cost.saturating_add(estimator.message(model, message).await?);
    }
    Ok(cost)
}
