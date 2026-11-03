use eliteforge_agent_core::*;
use std::{collections::BTreeMap, sync::Mutex};

/// Rotate provider bindings within one logical model/version. Candidate filtering,
/// authorization and embedding-space compatibility belong to the invoker/host.
#[derive(Default)]
pub struct RoundRobinAutoRouter {
    next: Mutex<BTreeMap<String, usize>>,
}

impl RoundRobinAutoRouter {
    fn select_candidate(
        &self,
        context: &AutoRouteContext<'_>,
        candidates: &[AutoRouteCandidate],
    ) -> Result<ModelRef, AgentError> {
        let previous = context.previous.and_then(|model| {
            candidates
                .iter()
                .find(|candidate| &candidate.model == model)
        });
        let logical = previous
            .or_else(|| candidates.first())
            .ok_or_else(super::exhausted)?
            .model_code
            .as_str();
        let mut pool = candidates
            .iter()
            .filter(|candidate| candidate.model_code == logical)
            .collect::<Vec<_>>();
        pool.sort_by(|left, right| {
            (&left.provider_id, &left.model).cmp(&(&right.provider_id, &right.model))
        });
        if context.transient_failures > 0
            && let Some(previous) = previous
        {
            if pool.len() == 1 {
                return Ok(pool[0].model.clone());
            }
            let position = pool
                .iter()
                .position(|candidate| candidate.model == previous.model)
                .expect("candidate is in its logical group");
            return Ok(pool[(position + 1) % pool.len()].model.clone());
        }
        let key = format!(
            "{:?}:{logical}:{:?}",
            context.operation,
            pool.iter()
                .map(|candidate| &candidate.model)
                .collect::<Vec<_>>()
        );
        let mut state = self.next.lock().unwrap_or_else(|error| error.into_inner());
        if state.len() >= 4096 && !state.contains_key(&key) {
            state.pop_first();
        }
        let next = state.entry(key).or_default();
        let selected = pool[*next % pool.len()].model.clone();
        if context.transient_failures == 0 {
            *next = next.wrapping_add(1);
        }
        Ok(selected)
    }
}

macro_rules! router {
    ($contract:ident) => {
        impl $contract for RoundRobinAutoRouter {
            fn select(
                &self,
                context: &AutoRouteContext<'_>,
                candidates: &[AutoRouteCandidate],
            ) -> Result<ModelRef, AgentError> {
                self.select_candidate(context, candidates)
            }
        }
    };
}
router!(ChatAutoRouter);
router!(TextGenerationAutoRouter);
router!(TextEmbeddingAutoRouter);
router!(MultimodalEmbeddingAutoRouter);
router!(RerankAutoRouter);
router!(MultimodalRerankAutoRouter);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotates_provider_replicas_without_switching_logical_models() {
        let policy = RoundRobinAutoRouter::default();
        let mut candidates = ["a", "b", "c"]
            .map(|provider| {
                let mut candidate = AutoRouteCandidate::new(ModelRef::new(provider, "binding"));
                candidate.model_code = "logical-v1".into();
                candidate
            })
            .to_vec();
        candidates.push(AutoRouteCandidate::new(ModelRef::new(
            "other",
            "incompatible-v2",
        )));
        let context = AutoRouteContext {
            operation: ModelOperation::Embedding,
            stage: "embed",
            affinity: "",
            call_key: "",
            previous: None,
            transient_failures: 0,
        };
        let observed = (0..4)
            .map(|_| {
                EmbeddingAutoRouter::select(&policy, &context, &candidates)
                    .unwrap()
                    .connection
            })
            .collect::<Vec<_>>();
        assert_eq!(observed, ["a", "b", "c", "a"]);
        candidates.swap(0, 2);
        assert_eq!(
            EmbeddingAutoRouter::select(&policy, &context, &candidates)
                .unwrap()
                .connection,
            "b"
        );
    }
}
