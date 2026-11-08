//! PersonAI policies consume host metadata; they never read host storage directly.
use eliteforge_agent_capability_auto_routing::{OrderedAutoRouter, StableAutoRouter};
use eliteforge_agent_core::*;

pub const PERSONAI_QUALITY: &str = "personai.quality";
pub const PERSONAI_PROVIDER: &str = "personai.provider";
pub use eliteforge_agent_capability_auto_routing::RunScopedFailoverAutoRouter;

#[derive(Clone, Copy, Default)]
pub enum PersonAiAutoRouteSelection {
    Ordered,
    #[default]
    Stable,
}

/// Shared PersonAI policy for 对话自动路由、向量自动路由、多模态向量自动路由、重排自动路由.
/// Quality is explicit host data: unknown quality cannot pass a High-only filter.
#[derive(Default)]
pub struct PersonAiAutoRouter {
    pub high_quality_only: bool,
    pub prefer_responses: bool,
    pub selection: PersonAiAutoRouteSelection,
}
impl PersonAiAutoRouter {
    /// Keep the current model first, then preserve/grow its context window
    /// before considering a smaller one. Hosts supply immutable model data.
    pub fn recovery_candidates(
        candidates: &[AutoRouteCandidate],
        current: &ModelRef,
        window: usize,
    ) -> Vec<AutoRouteCandidate> {
        let mut pool = candidates.to_vec();
        pool.sort_by_key(|candidate| {
            let size = candidate.context_window_tokens.unwrap_or_default();
            (
                candidate.model != *current,
                !is_personai(candidate),
                if size == window {
                    0
                } else if size > window {
                    1
                } else {
                    2
                },
                candidate.model.clone(),
            )
        });
        pool
    }
    pub fn candidates(&self, candidates: &[AutoRouteCandidate]) -> Vec<AutoRouteCandidate> {
        candidates
            .iter()
            .filter(|candidate| {
                !self.high_quality_only
                    || !is_personai(candidate)
                    || candidate
                        .metadata
                        .get(PERSONAI_QUALITY)
                        .and_then(|v| v.as_str())
                        == Some("High")
                    || candidate
                        .metadata
                        .get("personai.quality_tiers")
                        .and_then(|v| v.as_array())
                        .is_some_and(|tiers| {
                            tiers.iter().any(|tier| {
                                tier.as_str()
                                    .is_some_and(|tier| tier.eq_ignore_ascii_case("high"))
                            })
                        })
            })
            .cloned()
            .collect()
    }
    fn choose(
        &self,
        context: &AutoRouteContext<'_>,
        candidates: &[AutoRouteCandidate],
    ) -> Result<ModelRef, AgentError> {
        let mut pool = self.candidates(candidates);
        if context.transient_failures == 0
            && let Some(previous) = context.previous
            && pool.iter().any(|c| &c.model == previous)
        {
            return Ok(previous.clone());
        }
        if context.transient_failures == 0 && pool.iter().any(is_personai) {
            pool.retain(is_personai);
        }
        if self.prefer_responses && pool.iter().any(|c| c.protocol == "openai.responses") {
            pool.sort_by_key(|c| c.protocol != "openai.responses");
            if context.transient_failures == 0 {
                pool.retain(|c| c.protocol == "openai.responses");
            }
        }
        match self.selection {
            PersonAiAutoRouteSelection::Ordered => {
                ChatAutoRouter::select(&OrderedAutoRouter, context, &pool)
            }
            PersonAiAutoRouteSelection::Stable => {
                ChatAutoRouter::select(&StableAutoRouter, context, &pool)
            }
        }
    }
}
fn is_personai(candidate: &AutoRouteCandidate) -> bool {
    candidate
        .metadata
        .get(PERSONAI_PROVIDER)
        .and_then(|v| v.as_bool())
        == Some(true)
}
macro_rules! implement {
    ($contract:ident) => {
        impl $contract for PersonAiAutoRouter {
            fn candidates(&self, candidates: &[AutoRouteCandidate]) -> Vec<AutoRouteCandidate> {
                PersonAiAutoRouter::candidates(self, candidates)
            }
            fn select(
                &self,
                context: &AutoRouteContext<'_>,
                candidates: &[AutoRouteCandidate],
            ) -> Result<ModelRef, AgentError> {
                self.choose(context, candidates)
            }
        }
    };
}
implement!(ChatAutoRouter);

/// Direct text generation has its own candidate policy and no conversation
/// assignment. Prefer declared Middle candidates, then Low candidates.
#[derive(Default)]
pub struct PersonAiTextGenerationAutoRouter;
impl TextGenerationAutoRouter for PersonAiTextGenerationAutoRouter {
    fn candidates(&self, candidates: &[AutoRouteCandidate]) -> Vec<AutoRouteCandidate> {
        personai_text_generation_candidates(candidates)
    }
    fn select(
        &self,
        context: &AutoRouteContext<'_>,
        candidates: &[AutoRouteCandidate],
    ) -> Result<ModelRef, AgentError> {
        TextGenerationAutoRouter::select(
            &OrderedAutoRouter,
            context,
            &personai_text_generation_candidates(candidates),
        )
    }
}

/// PersonAI supplies only explicitly declared Middle/Low chat models to the
/// text-generation route. This never promotes unknown or High quality models.
pub fn personai_text_generation_candidates(
    candidates: &[AutoRouteCandidate],
) -> Vec<AutoRouteCandidate> {
    let mut pool = candidates
        .iter()
        .filter(|candidate| {
            is_personai(candidate) && text_generation_quality_rank(candidate).is_some()
        })
        .cloned()
        .collect::<Vec<_>>();
    pool.sort_by_key(|candidate| {
        (
            text_generation_quality_rank(candidate),
            candidate.model.clone(),
        )
    });
    pool
}
fn text_generation_quality_rank(candidate: &AutoRouteCandidate) -> Option<u8> {
    if let Some(quality) = candidate.metadata.get(PERSONAI_QUALITY) {
        return match quality.as_str() {
            Some("Middle") => Some(0),
            Some("Low") => Some(1),
            _ => None,
        };
    }
    let tiers = candidate
        .metadata
        .get("personai.quality_tiers")?
        .as_array()?;
    // Native discovery binds the lowest declared tier (see chat_quality).
    if tiers.iter().any(|tier| tier.as_str() == Some("low")) {
        Some(1)
    } else if tiers.iter().any(|tier| tier.as_str() == Some("middle")) {
        Some(0)
    } else {
        None
    }
}
impl TextEmbeddingAutoRouter for PersonAiAutoRouter {
    fn select(
        &self,
        context: &AutoRouteContext<'_>,
        candidates: &[AutoRouteCandidate],
    ) -> Result<ModelRef, AgentError> {
        self.choose(context, candidates)
    }
}
impl MultimodalEmbeddingAutoRouter for PersonAiAutoRouter {
    fn select(
        &self,
        context: &AutoRouteContext<'_>,
        candidates: &[AutoRouteCandidate],
    ) -> Result<ModelRef, AgentError> {
        self.choose(context, candidates)
    }
}
implement!(RerankAutoRouter);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_discovery_tiers_match_the_bound_quality_and_cannot_override_explicit_high() {
        let mut native = AutoRouteCandidate::new(ModelRef::new("personai", "native"));
        native
            .metadata
            .insert(PERSONAI_PROVIDER.into(), true.into());
        native.metadata.insert(
            "personai.quality_tiers".into(),
            serde_json::json!(["high", "middle"]),
        );
        assert_eq!(text_generation_quality_rank(&native), Some(0));
        assert_eq!(
            personai_text_generation_candidates(&[native.clone()]).len(),
            1
        );
        native.metadata.insert(
            "personai.quality_tiers".into(),
            serde_json::json!(["high", "middle", "low"]),
        );
        assert_eq!(text_generation_quality_rank(&native), Some(1));
        native
            .metadata
            .insert(PERSONAI_QUALITY.into(), "High".into());
        assert!(personai_text_generation_candidates(&[native]).is_empty());
    }
    #[test]
    fn text_generation_supplies_only_declared_middle_and_low_models() {
        let model = |name: &str, quality: Option<&str>, provider: bool| {
            let mut candidate = AutoRouteCandidate::new(ModelRef::new("personai", name));
            candidate
                .metadata
                .insert(PERSONAI_PROVIDER.into(), provider.into());
            if let Some(quality) = quality {
                candidate
                    .metadata
                    .insert(PERSONAI_QUALITY.into(), quality.into());
            }
            candidate
        };
        let pool = personai_text_generation_candidates(&[
            model("low", Some("Low"), true),
            model("high", Some("High"), true),
            model("unknown", None, true),
            model("foreign", Some("Middle"), false),
            model("middle", Some("Middle"), true),
        ]);
        assert_eq!(
            pool.iter()
                .map(|candidate| candidate.model.model.as_str())
                .collect::<Vec<_>>(),
            ["middle", "low"]
        );
        assert!(
            personai_text_generation_candidates(&[model("high", Some("High"), true)]).is_empty()
        );
    }

    #[test]
    fn quality_filter_is_host_driven_and_never_guesses_unknown_quality() {
        let mut low = AutoRouteCandidate::new(ModelRef::new("personai", "low"));
        low.metadata.insert(PERSONAI_PROVIDER.into(), true.into());
        low.metadata.insert(PERSONAI_QUALITY.into(), "Low".into());
        let mut high = low.clone();
        high.model.model = "high".into();
        high.metadata.insert(PERSONAI_QUALITY.into(), "High".into());
        let mut unknown = low.clone();
        unknown.metadata.remove(PERSONAI_QUALITY);
        let router = PersonAiAutoRouter {
            high_quality_only: true,
            ..Default::default()
        };
        assert_eq!(
            router.candidates(&[low.clone(), unknown.clone(), high.clone()])[0].model,
            high.model
        );
        assert!(router.candidates(&[low, unknown]).is_empty());
        let local = AutoRouteCandidate::new(ModelRef::new("host", "local"));
        assert_eq!(router.candidates(&[local.clone(), high.clone()]).len(), 2);
        assert_eq!(router.candidates(&[local.clone()])[0].model, local.model);
    }
}
