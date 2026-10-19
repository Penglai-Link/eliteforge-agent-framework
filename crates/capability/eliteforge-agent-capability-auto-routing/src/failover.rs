use eliteforge_agent_core::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

/// One instance per run. Failure tracking and sticky selection survive step
/// changes and compatibility filtering; execution/retries stay in Framework.
pub struct RunScopedFailoverAutoRouter {
    pub max_transient_failures: usize,
    state: Mutex<SelectionState>,
}
impl Default for RunScopedFailoverAutoRouter {
    fn default() -> Self {
        Self {
            max_transient_failures: 2,
            state: Mutex::default(),
        }
    }
}
#[derive(Default)]
struct SelectionState {
    failed: BTreeSet<ModelRef>,
    current: Option<ModelRef>,
    retries: BTreeMap<String, usize>,
}
impl ChatAutoRouter for RunScopedFailoverAutoRouter {
    fn select(
        &self,
        context: &AutoRouteContext<'_>,
        candidates: &[AutoRouteCandidate],
    ) -> Result<ModelRef, AgentError> {
        if context.transient_failures > self.max_transient_failures {
            return Err(AgentError::Run(
                "对话自动路由 fallback allowance exhausted".into(),
            ));
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if context.transient_failures
            > state
                .retries
                .get(context.call_key)
                .copied()
                .unwrap_or_default()
            && let Some(previous) = context.previous
        {
            state.failed.insert(previous.clone());
        }
        state
            .retries
            .insert(context.call_key.into(), context.transient_failures);
        let selected = state
            .current
            .as_ref()
            .filter(|model| {
                !state.failed.contains(*model) && candidates.iter().any(|c| &c.model == *model)
            })
            .or_else(|| {
                candidates
                    .iter()
                    .map(|c| &c.model)
                    .find(|model| !state.failed.contains(*model))
            })
            .cloned()
            .ok_or_else(|| {
                AgentError::Run("对话自动路由 has no remaining compatible candidates".into())
            })?;
        state.current = Some(selected.clone());
        Ok(selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn choose(
        router: &RunScopedFailoverAutoRouter,
        models: &[AutoRouteCandidate],
        previous: Option<&ModelRef>,
        retries: usize,
        call: &str,
    ) -> Option<ModelRef> {
        router
            .select(
                &AutoRouteContext {
                    operation: ModelOperation::Chat,
                    stage: "",
                    affinity: "session",
                    call_key: call,
                    previous,
                    transient_failures: retries,
                },
                models,
            )
            .ok()
    }
    #[test]
    fn sticky_model_survives_steps_and_failed_models_are_not_revisited() {
        let models =
            ["first", "second", "third"].map(|m| AutoRouteCandidate::new(ModelRef::new("host", m)));
        let router = RunScopedFailoverAutoRouter::default();
        assert_eq!(
            choose(&router, &models, None, 0, "step"),
            Some(models[0].model.clone())
        );
        assert_eq!(
            choose(&router, &models, Some(&models[0].model), 1, "step"),
            Some(models[1].model.clone())
        );
        assert_eq!(
            choose(&router, &models, Some(&models[1].model), 2, "step"),
            Some(models[2].model.clone())
        );
        assert_eq!(
            choose(&router, &models, None, 0, "next-step"),
            Some(models[2].model.clone())
        );
        assert_eq!(
            choose(&router, &models, Some(&models[2].model), 1, "next-step"),
            None
        );
    }
    #[test]
    fn context_recovery_and_sibling_nodes_do_not_consume_another_candidate() {
        let models = ["first", "second"].map(|m| AutoRouteCandidate::new(ModelRef::new("host", m)));
        let router = RunScopedFailoverAutoRouter::default();
        choose(&router, &models, None, 0, "step");
        assert_eq!(
            choose(&router, &models, Some(&models[0].model), 1, "step"),
            Some(models[1].model.clone())
        );
        choose(&router, &models, None, 0, "parallel-node");
        assert_eq!(
            choose(&router, &models, Some(&models[1].model), 1, "step"),
            Some(models[1].model.clone())
        );
    }
    #[test]
    fn shrinking_compatibility_list_does_not_skip_the_remaining_model() {
        let first = ModelRef::new("host", "first");
        let remaining = AutoRouteCandidate::new(ModelRef::new("host", "remaining"));
        let router = RunScopedFailoverAutoRouter::default();
        assert_eq!(
            choose(&router, &[remaining.clone()], Some(&first), 1, "step"),
            Some(remaining.model)
        );
    }
}
