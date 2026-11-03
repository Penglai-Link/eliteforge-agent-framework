//! Automatic routing policies. Hosts supply data; the Framework owns dispatch.
use eliteforge_agent_core::*;
use std::sync::Arc;
mod failover;
pub use failover::RunScopedFailoverAutoRouter;
mod round_robin;
pub use round_robin::RoundRobinAutoRouter;

/// Use host candidate order, advancing only on transient failures.
pub struct OrderedAutoRouter;
/// Stable assignment by affinity and model identity, independent of input order.
pub struct StableAutoRouter;

fn ordered(
    context: &AutoRouteContext<'_>,
    candidates: &[AutoRouteCandidate],
) -> Result<ModelRef, AgentError> {
    candidates
        .get(context.transient_failures)
        .map(|c| c.model.clone())
        .ok_or_else(exhausted)
}
fn stable(
    context: &AutoRouteContext<'_>,
    candidates: &[AutoRouteCandidate],
) -> Result<ModelRef, AgentError> {
    if context.transient_failures == 0
        && let Some(previous) = context.previous
        && candidates.iter().any(|c| &c.model == previous)
    {
        return Ok(previous.clone());
    }
    let mut pool = candidates.iter().collect::<Vec<_>>();
    pool.sort_by_key(|c| {
        (
            std::cmp::Reverse(score(context.affinity, &c.model)),
            c.model.clone(),
        )
    });
    pool.into_iter()
        .skip(context.transient_failures)
        .find(|c| context.transient_failures == 0 || context.previous != Some(&c.model))
        .map(|c| c.model.clone())
        .ok_or_else(exhausted)
}
fn exhausted() -> AgentError {
    AgentError::InvalidConfiguration("automatic route has no remaining candidates".into())
}
fn score(affinity: &str, model: &ModelRef) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in [affinity, &model.connection, &model.model]
        .into_iter()
        .flat_map(|part| part.bytes().chain([0xff]))
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
macro_rules! implement {
    ($router:ident, $select:ident) => {
        impl TextGenerationAutoRouter for $router {
            fn select(
                &self,
                context: &AutoRouteContext<'_>,
                candidates: &[AutoRouteCandidate],
            ) -> Result<ModelRef, AgentError> {
                $select(context, candidates)
            }
        }
        impl ChatAutoRouter for $router {
            fn select(
                &self,
                context: &AutoRouteContext<'_>,
                candidates: &[AutoRouteCandidate],
            ) -> Result<ModelRef, AgentError> {
                $select(context, candidates)
            }
        }
        impl TextEmbeddingAutoRouter for $router {
            fn select(
                &self,
                context: &AutoRouteContext<'_>,
                candidates: &[AutoRouteCandidate],
            ) -> Result<ModelRef, AgentError> {
                $select(context, candidates)
            }
        }
        impl MultimodalRerankAutoRouter for $router {
            fn select(
                &self,
                context: &AutoRouteContext<'_>,
                candidates: &[AutoRouteCandidate],
            ) -> Result<ModelRef, AgentError> {
                $select(context, candidates)
            }
        }
        impl RerankAutoRouter for $router {
            fn select(
                &self,
                context: &AutoRouteContext<'_>,
                candidates: &[AutoRouteCandidate],
            ) -> Result<ModelRef, AgentError> {
                $select(context, candidates)
            }
        }
        impl MultimodalEmbeddingAutoRouter for $router {
            fn select(
                &self,
                context: &AutoRouteContext<'_>,
                candidates: &[AutoRouteCandidate],
            ) -> Result<ModelRef, AgentError> {
                $select(context, candidates)
            }
        }
    };
}
implement!(OrderedAutoRouter, ordered);
implement!(StableAutoRouter, stable);

/// Explicit installation: no policy silently replaces an existing selection.
pub struct AutoRoutingCapabilityPack(pub AutoRouters);
impl CapabilityPack for AutoRoutingCapabilityPack {
    fn id(&self) -> &str {
        "auto-routing"
    }
    fn configuration(&self) -> &CapabilityConfig {
        &()
    }
    fn plugins(&self) -> Vec<Arc<dyn Plugin>> {
        vec![Arc::new(AutoRoutersPlugin(self.0.clone()))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_assignment_survives_catalog_reordering_and_honors_previous() {
        let a = AutoRouteCandidate::new(ModelRef::new("host", "a"));
        let b = AutoRouteCandidate::new(ModelRef::new("host", "b"));
        let mut context = AutoRouteContext {
            operation: ModelOperation::Chat,
            stage: "",
            affinity: "ws/session/agent",
            call_key: "step",
            previous: None,
            transient_failures: 0,
        };
        let selected =
            ChatAutoRouter::select(&StableAutoRouter, &context, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(
            selected,
            ChatAutoRouter::select(&StableAutoRouter, &context, &[b.clone(), a.clone()]).unwrap()
        );
        context.previous = Some(&a.model);
        assert_eq!(
            a.model,
            ChatAutoRouter::select(&StableAutoRouter, &context, &[b.clone(), a.clone()]).unwrap()
        );
        context.previous = None;
        context.transient_failures = 1;
        assert_ne!(
            selected,
            ChatAutoRouter::select(&StableAutoRouter, &context, &[a.clone(), b]).unwrap()
        );
    }
    #[test]
    fn unified_rerank_policy_keeps_operation_guards_and_accepts_legacy_entry_points() {
        let available = vec![AutoRouteCandidate::new(ModelRef::new("host", "multi"))];
        let routers = AutoRouters {
            rerank: Some(Arc::new(OrderedAutoRouter)),
            ..Default::default()
        };
        let context = AutoRouteContext {
            operation: ModelOperation::Rerank,
            stage: "rerank",
            affinity: "workspace",
            call_key: "rerank",
            previous: None,
            transient_failures: 0,
        };
        assert_eq!(
            routers
                .select_multimodal_rerank(&context, &available)
                .unwrap(),
            available[0].model
        );
        let routers = AutoRouters {
            multimodal_rerank: Some(Arc::new(StableAutoRouter)),
            ..Default::default()
        };
        assert_eq!(
            routers
                .select_multimodal_rerank(&context, &available)
                .unwrap(),
            available[0].model
        );
        assert_eq!(
            routers.select(&context, &available).unwrap(),
            available[0].model
        );
        let wrong = AutoRouteContext {
            operation: ModelOperation::Embedding,
            ..context
        };
        assert!(
            routers
                .select_multimodal_rerank(&wrong, &available)
                .is_err()
        );
    }
    #[test]
    fn ordered_routers_exhaust_without_reusing_last_candidate() {
        let candidates = vec![AutoRouteCandidate::new(ModelRef::new("host", "a"))];
        for operation in [
            ModelOperation::Chat,
            ModelOperation::Embedding,
            ModelOperation::Rerank,
        ] {
            let routers = AutoRouters {
                chat: Some(Arc::new(OrderedAutoRouter)),
                embedding: Some(Arc::new(OrderedAutoRouter)),
                rerank: Some(Arc::new(OrderedAutoRouter)),
                ..Default::default()
            };
            let mut context = AutoRouteContext {
                operation,
                stage: "",
                affinity: "",
                call_key: "step",
                previous: None,
                transient_failures: 0,
            };
            assert_eq!(
                routers.select(&context, &candidates).unwrap(),
                candidates[0].model
            );
            context.transient_failures = 1;
            assert!(routers.select(&context, &candidates).is_err());
        }
    }
}
